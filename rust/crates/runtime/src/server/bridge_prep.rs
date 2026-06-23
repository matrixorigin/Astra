use super::*;
use astra_services::runs::SelectedModelRequest;

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
    selected_model: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_state: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_turn: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_chain_id: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_query_event_id: Option<serde_json::Value>,
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

    fn selected_model_obj(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.selected_model.as_ref()?.as_object()
    }

    fn selected_model_name(&self) -> Option<&str> {
        self.selected_model_obj()?.get("model")?.as_str()
    }

    fn selected_model_gateway(&self) -> Option<&str> {
        self.selected_model_obj()?
            .get("gateway")
            .and_then(serde_json::Value::as_str)
    }

    fn execution_state_obj(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.execution_state.as_ref()?.as_object()
    }

    fn explicit_session_turn(&self) -> Option<u32> {
        self.session_turn
            .as_ref()?
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
    }

    fn explicit_turn_chain_id(&self) -> Option<&str> {
        self.turn_chain_id
            .as_ref()?
            .as_str()
            .filter(|value| !value.trim().is_empty())
    }

    fn explicit_user_query_event_id(&self) -> Option<&str> {
        self.user_query_event_id
            .as_ref()?
            .as_str()
            .filter(|value| !value.trim().is_empty())
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
/// Delegates to the single-source admission contract
/// (`astra_turn_core::tool::schema::tool_schema_name`) so explicit
/// non-function tool types (custom MCP shapes, future reserved types) and
/// malformed entries fail closed, while provider shorthand
/// `{function:{name:...}}` schemas are still admitted.
fn extract_tool_name(tool: &serde_json::Value) -> Option<&str> {
    astra_turn_core::tool::schema::tool_schema_name(tool)
}

// ─── Prepared result ─────────────────────────────────────────────────────────

pub(super) struct PreparedChatTurnBridgeRequest {
    pub(super) body: Bytes,
    pub(super) trusted_session_id: Option<String>,
    pub(super) full_llm_capture: Option<bool>,
    pub(super) session_turn: Option<String>,
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
            full_llm_capture: None,
            session_turn: None,
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

fn validate_exact_bridge_string(
    field: &'static str,
    value: &str,
    code: &'static str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            format!(
                "{field} must be a non-empty exact string without leading/trailing whitespace or control characters"
            ),
            code,
        ));
    }
    Ok(())
}

fn validate_selected_model_shape(
    request: &ChatTurnRequestBody,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if request.model.is_some() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "legacy top-level model is not accepted on /chat/turn; use selected_model.model",
            "selected_model_invalid",
        ));
    }
    let Some(selected_model) = request.selected_model.as_ref() else {
        tracing::warn!(
            target: "astra_runtime::server::chat_turn_bridge",
            reason = "missing_model_selection",
            "selected_model.model is required before /chat/turn can create or bind a session"
        );
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            astra_core::model_override::MISSING_MODEL_SELECTION_MESSAGE,
            "missing_model_selection",
        ));
    };
    let Some(obj) = selected_model.as_object() else {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "selected_model must be an object",
            "selected_model_invalid",
        ));
    };
    // Delegate field whitelist to SelectedModelRequest's deny_unknown_fields.
    if serde_json::from_value::<SelectedModelRequest>(selected_model.clone()).is_err() {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "selected_model must match {model: string, gateway?: string}",
            "selected_model_invalid",
        ));
    }
    let Some(model) = request.selected_model_name() else {
        return Err(error_response_coded(
            StatusCode::BAD_REQUEST,
            "selected_model.model is required",
            "selected_model_invalid",
        ));
    };
    validate_exact_bridge_string("selected_model.model", model, "selected_model_invalid")?;
    if obj.get("gateway").is_some_and(|value| !value.is_null()) {
        let Some(gateway) = request.selected_model_gateway() else {
            return Err(error_response_coded(
                StatusCode::BAD_REQUEST,
                "selected_model.gateway must be a string when provided",
                "selected_model_invalid",
            ));
        };
        validate_exact_bridge_string("selected_model.gateway", gateway, "selected_model_invalid")?;
    }
    Ok(())
}

#[derive(Clone)]
struct ExplicitBridgeTurnIdentity {
    session_turn: u32,
    turn_chain_id: String,
    user_query_event_id: String,
}

fn validate_explicit_turn_identity(
    request: &ChatTurnRequestBody,
) -> Result<Option<ExplicitBridgeTurnIdentity>, (StatusCode, Json<ErrorResponse>)> {
    let any_present = request.session_turn.is_some()
        || request.turn_chain_id.is_some()
        || request.user_query_event_id.is_some();
    if !any_present {
        return Ok(None);
    }
    let Some(session_turn) = request.explicit_session_turn() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "session_turn must be a positive integer when explicit bridge identity is provided",
        ));
    };
    let Some(turn_chain_id) = request.explicit_turn_chain_id() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "turn_chain_id must be a non-empty string when explicit bridge identity is provided",
        ));
    };
    let Some(user_query_event_id) = request.explicit_user_query_event_id() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "user_query_event_id must be a non-empty string when explicit bridge identity is provided",
        ));
    };
    Ok(Some(ExplicitBridgeTurnIdentity {
        session_turn,
        turn_chain_id: turn_chain_id.to_string(),
        user_query_event_id: user_query_event_id.to_string(),
    }))
}

pub(super) async fn prepare_chat_turn_bridge_body(
    state: &AppState,
    user: &AuthUserRecord,
    body: Bytes,
    trusted_session_id_override: Option<&str>,
) -> Result<PreparedChatTurnBridgeRequest, (StatusCode, Json<ErrorResponse>)> {
    let Ok(mut request) = serde_json::from_slice::<ChatTurnRequestBody>(&body) else {
        return Ok(PreparedChatTurnBridgeRequest::passthrough(body));
    };
    validate_selected_model_shape(&request)?;
    validate_session_id_shape(&request)?;
    let explicit_turn_identity = validate_explicit_turn_identity(&request)?;

    // ── Session resolution ──────────────────────────────────────────────
    let (trusted_session_id, trusted_session_created_at, full_llm_capture) =
        if let Some(session_id) = request.session_id_str().map(String::from) {
            let session = state
                .session_service
                .get_session(session_id.clone(), user.user_id.clone())
                .await
                .map_err(normalize_chat_turn_session_error)?;
            (
                Some(session_id),
                normalize_session_created_at_for_bridge(&session.created_at),
                crate::turn::llm::exchange_capture::session_full_llm_capture_enabled(Some(
                    &session.metadata,
                )),
            )
        } else if let Some(session_id) = trusted_session_id_override {
            request.set_session_id(session_id);
            (Some(session_id.to_string()), None, false)
        } else {
            let agent_id = request.agent_id_str();
            let metadata = agent_id.as_ref().map(|agent_id| {
                serde_json::Map::from_iter([(
                    "agent_id".to_string(),
                    serde_json::Value::String(agent_id.clone()),
                )])
            });
            let session = super::session::session_quota::create_session_with_resource_quota(
                state,
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
            (
                Some(created_session_id),
                created_session_created_at,
                crate::turn::llm::exchange_capture::session_full_llm_capture_enabled(Some(
                    &session.metadata,
                )),
            )
        };

    if let (Some(session_id), Some(created_at)) = (
        trusted_session_id.as_deref(),
        trusted_session_created_at.as_deref(),
    ) {
        seed_bridge_session_created_at(state, &user.user_id, session_id, created_at).await;
    }
    // ── Turn identifiers ────────────────────────────────────────────────
    let (turn_chain_id, user_query_event_id, session_turn) =
        if let Some(session_id) = trusted_session_id.as_deref() {
            let messages = request.messages_slice();
            let has_tool_results = request.has_tool_results();
            let (chain_id, event_id, session_turn) = prepare_chat_turn_bridge_identifiers(
                state,
                &user.user_id,
                session_id,
                messages,
                has_tool_results,
                explicit_turn_identity.as_ref(),
            )
            .await;
            (
                Some(chain_id),
                Some(event_id),
                Some(session_turn.to_string()),
            )
        } else {
            (None, None, None)
        };

    // ── Cached inputs + tool trimming ───────────────────────────────────
    let tools_changed = if let Some(session_id) = trusted_session_id.as_deref() {
        Some(
            prepare_chat_turn_bridge_cached_inputs(state, &user.user_id, session_id, &mut request)
                .await,
        )
    } else {
        None
    };

    let user_query = request.user_query();
    // Tool selection is client-side; server trusts pre-selected set.
    trim_edge_tools_for_result_turn(&mut request, &user_query);

    // ── Metadata extraction ─────────────────────────────────────────────
    let task_hint = request.classify_task();
    let user_query_b64 = Some(URL_SAFE.encode(user_query.as_bytes()));
    let routing_meta_b64 = request.selected_model_name().map(|_| {
        let meta = serde_json::Value::Object(build_skipped_routing_metadata("selected_model"));
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
            full_llm_capture: full_llm_capture.then_some(true),
            session_turn,
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

fn bridge_cache_key(user_id: &str, session_id: &str) -> String {
    format!("{user_id}\x1f{session_id}")
}

async fn infer_bridge_session_turn(state: &AppState, user_id: &str, session_id: &str) -> u32 {
    let Some(shared_pool) = state.shared_pool.as_ref() else {
        return 1;
    };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'user_query'",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(shared_pool.get())
    .await
    .unwrap_or(0);
    (count.max(0) as u32).saturating_add(1)
}

async fn seed_bridge_session_created_at(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    created_at: &str,
) {
    if created_at.is_empty() {
        return;
    }
    let now = current_unix_seconds();
    let cache_key = bridge_cache_key(user_id, session_id);
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut entry = cache.get(&cache_key, now).unwrap_or_default();
    entry
        .entry("created_at".to_string())
        .or_insert_with(|| serde_json::Value::String(created_at.to_string()));
    cache.insert(cache_key, entry, now);
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

pub(super) fn normalize_chat_turn_session_error(
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
    user_id: &str,
    session_id: &str,
    messages: &[serde_json::Value],
    has_tool_results: bool,
    explicit_identity: Option<&ExplicitBridgeTurnIdentity>,
) -> (String, String, u32) {
    if let Some(identity) = explicit_identity {
        let now = current_unix_seconds();
        let cache_key = bridge_cache_key(user_id, session_id);
        let mut cache = state.chat_turn_bridge_cache.lock().await;
        let mut updated_entry = cache.get(&cache_key, now).unwrap_or_default();
        updated_entry.insert(
            "turn_chain_id".to_string(),
            serde_json::Value::String(identity.turn_chain_id.clone()),
        );
        updated_entry.insert(
            "user_query_event_id".to_string(),
            serde_json::Value::String(identity.user_query_event_id.clone()),
        );
        updated_entry.insert(
            "session_turn".to_string(),
            serde_json::json!(identity.session_turn),
        );
        cache.insert(cache_key, updated_entry, now);
        return (
            identity.turn_chain_id.clone(),
            identity.user_query_event_id.clone(),
            identity.session_turn,
        );
    }
    let inferred_session_turn = infer_bridge_session_turn(state, user_id, session_id).await;
    let now = current_unix_seconds();
    let cache_key = bridge_cache_key(user_id, session_id);
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut prev_entry = cache.get(&cache_key, now);
    let is_continuation = bridge_turn_is_continuation(messages, has_tool_results);
    let new_turn_chain_id = Uuid::now_v7().to_string();
    let new_user_query_event_id = Uuid::now_v7().to_string();
    let (turn_chain_id, user_query_event_id) = resolve_turn_identifiers(
        messages,
        has_tool_results,
        prev_entry.as_mut(),
        &new_turn_chain_id,
        &new_user_query_event_id,
    );
    let session_turn = if is_continuation {
        prev_entry
            .as_ref()
            .and_then(|entry| entry.get("session_turn"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|turn| u32::try_from(turn).ok())
            .filter(|turn| *turn > 0)
            .unwrap_or(inferred_session_turn)
    } else {
        inferred_session_turn
    };
    let mut updated_entry = prev_entry.unwrap_or_default();
    updated_entry.insert(
        "turn_chain_id".to_string(),
        serde_json::Value::String(turn_chain_id.clone()),
    );
    updated_entry.insert(
        "user_query_event_id".to_string(),
        serde_json::Value::String(user_query_event_id.clone()),
    );
    updated_entry.insert("session_turn".to_string(), serde_json::json!(session_turn));
    cache.insert(cache_key, updated_entry, now);
    (turn_chain_id, user_query_event_id, session_turn)
}

fn bridge_turn_is_continuation(messages: &[serde_json::Value], has_tool_results: bool) -> bool {
    let latest_conversation_role = messages.iter().rev().find_map(|message| {
        match message.get("role").and_then(serde_json::Value::as_str) {
            Some("user" | "assistant" | "tool") => {
                message.get("role").and_then(serde_json::Value::as_str)
            }
            _ => None,
        }
    });
    latest_conversation_role != Some("user") && has_tool_results
}

async fn prepare_chat_turn_bridge_cached_inputs(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    request: &mut ChatTurnRequestBody,
) -> bool {
    let now = current_unix_seconds();
    let cache_key = bridge_cache_key(user_id, session_id);
    let mut cache = state.chat_turn_bridge_cache.lock().await;
    let mut entry = cache.get(&cache_key, now).unwrap_or_default();
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

    cache.insert(cache_key, entry, now);
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
    use async_trait::async_trait;
    use axum::{Json, http::StatusCode};
    use serde_json::json;
    use std::sync::Arc;

    use crate::{
        AppState, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, SessionActivityRecord,
        SessionCreateRequestData, SessionListFilter, SessionListRecord, SessionRecord,
        SessionService, SessionUpdateRequestData,
    };

    #[derive(Clone)]
    struct StubHealthChecker;

    #[async_trait]
    impl HealthChecker for StubHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    fn test_user() -> AuthUserRecord {
        test_user_with_id("u1")
    }

    fn test_user_with_id(user_id: &str) -> AuthUserRecord {
        AuthUserRecord {
            user_id: user_id.to_string(),
            username: format!("test-{user_id}"),
            email: format!("{user_id}@example.test"),
            display_name: None,
        }
    }

    fn tool_value(name: &str) -> serde_json::Value {
        // Mirrors the OpenAI function-tool schema shape required by the
        // single-source admission contract (`tool_schema_name`):
        // `type: "function"` + non-empty `function.name`.
        json!({ "type": "function", "function": { "name": name } })
    }

    fn selected_model() -> serde_json::Value {
        json!({"model": "deepseek-v4-pro-official"})
    }

    #[derive(Clone)]
    struct CaptureEnabledSessionService;

    #[async_trait]
    impl SessionService for CaptureEnabledSessionService {
        async fn create_session(
            &self,
            user_id: String,
            request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id: "capture-created".to_string(),
                user_id,
                agent_id: request.agent_id,
                title: Some("Created".to_string()),
                metadata: serde_json::Map::from_iter([(
                    crate::turn::llm::exchange_capture::FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
                    json!(true),
                )]),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: Some("Existing".to_string()),
                metadata: serde_json::Map::from_iter([(
                    crate::turn::llm::exchange_capture::FULL_LLM_CAPTURE_METADATA_KEY.to_string(),
                    json!(true),
                )]),
                status: "active".to_string(),
                event_count: 0,
                created_at: "2026-01-01T00:00:00".to_string(),
                updated_at: Some("2026-01-01T00:00:00".to_string()),
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            session_id: String,
            user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            self.get_session(session_id, user_id).await
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            Ok(())
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _offset: u32,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            Ok(SessionActivityRecord {
                session_id: String::new(),
                activities: vec![],
                total: 0,
            })
        }
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

    #[tokio::test]
    async fn prepare_body_uses_trusted_session_override_when_body_omits_session_id() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("body should serialize"),
        );

        let prepared =
            prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
                .expect("trusted override should bypass unconfigured session service");

        assert_eq!(
            prepared.trusted_session_id.as_deref(),
            Some("bound-session")
        );
        assert_eq!(prepared.session_turn.as_deref(), Some("1"));
        assert!(prepared.turn_chain_id.is_some());
        assert!(prepared.user_query_event_id.is_some());
        let payload: serde_json::Value =
            serde_json::from_slice(&prepared.body).expect("prepared body should be valid json");
        assert_eq!(payload["session_id"], "bound-session");
        assert_eq!(
            payload["selected_model"]["model"],
            "deepseek-v4-pro-official"
        );
        assert!(payload.get("model").is_none());
    }

    #[tokio::test]
    async fn prepare_body_rejects_missing_selected_model_before_session_side_effects() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("body should serialize"),
        );

        let (status, body) =
            match prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
            {
                Ok(_) => panic!("missing selected_model must fail before session preparation"),
                Err(error) => error,
            };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0.error_code.as_deref(),
            Some("missing_model_selection")
        );
        assert!(
            body.0.detail.contains("Model selection is required"),
            "{}",
            body.0.detail
        );
    }

    #[tokio::test]
    async fn prepare_body_rejects_legacy_top_level_model() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "deepseek-v4-pro-official",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("body should serialize"),
        );

        let (status, body) =
            match prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
            {
                Ok(_) => panic!("legacy model must not be accepted as selected_model"),
                Err(error) => error,
            };

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0.error_code.as_deref(), Some("selected_model_invalid"));
        assert!(body.0.detail.contains("legacy top-level model"));
    }

    #[tokio::test]
    async fn prepare_body_reuses_cached_session_turn_for_continuation() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let now = current_unix_seconds();
        {
            let mut cache = state.chat_turn_bridge_cache.lock().await;
            let mut entry = serde_json::Map::new();
            entry.insert("turn_chain_id".to_string(), json!("chain-6"));
            entry.insert("user_query_event_id".to_string(), json!("query-6"));
            entry.insert("session_turn".to_string(), json!(6));
            cache.insert(bridge_cache_key("u1", "bound-session"), entry, now);
        }
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "messages": [
                    {"role": "assistant", "content": null, "tool_calls": [{"id": "call-1"}]},
                    {"role": "tool", "tool_call_id": "call-1", "content": "done"}
                ],
                "tool_results": [{"name": "bash", "output": "done"}]
            }))
            .expect("body should serialize"),
        );

        let prepared =
            prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
                .expect("continuation should prepare");

        assert_eq!(prepared.session_turn.as_deref(), Some("6"));
        assert_eq!(prepared.turn_chain_id.as_deref(), Some("chain-6"));
        assert_eq!(prepared.user_query_event_id.as_deref(), Some("query-6"));
    }

    #[tokio::test]
    async fn prepare_body_does_not_reuse_cached_identity_from_another_owner() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let now = current_unix_seconds();
        {
            let mut cache = state.chat_turn_bridge_cache.lock().await;
            let mut entry = serde_json::Map::new();
            entry.insert("turn_chain_id".to_string(), json!("other-chain"));
            entry.insert("user_query_event_id".to_string(), json!("other-query"));
            entry.insert("session_turn".to_string(), json!(8));
            cache.insert(bridge_cache_key("other-user", "bound-session"), entry, now);
        }
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "messages": [
                    {"role": "assistant", "content": null, "tool_calls": [{"id": "call-1"}]},
                    {"role": "tool", "tool_call_id": "call-1", "content": "done"}
                ],
                "tool_results": [{"name": "bash", "output": "done"}]
            }))
            .expect("body should serialize"),
        );

        let prepared = prepare_chat_turn_bridge_body(
            &state,
            &test_user_with_id("current-user"),
            body,
            Some("bound-session"),
        )
        .await
        .expect("continuation should prepare");

        assert_eq!(prepared.session_turn.as_deref(), Some("1"));
        assert_ne!(prepared.turn_chain_id.as_deref(), Some("other-chain"));
        assert_ne!(prepared.user_query_event_id.as_deref(), Some("other-query"));
    }

    #[tokio::test]
    async fn prepare_body_prefers_explicit_payload_turn_identity() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let now = current_unix_seconds();
        {
            let mut cache = state.chat_turn_bridge_cache.lock().await;
            let mut entry = serde_json::Map::new();
            entry.insert("turn_chain_id".to_string(), json!("cached-chain"));
            entry.insert("user_query_event_id".to_string(), json!("cached-query"));
            entry.insert("session_turn".to_string(), json!(9));
            cache.insert(bridge_cache_key("u1", "bound-session"), entry, now);
        }
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "session_turn": 2,
                "turn_chain_id": "root-chain",
                "user_query_event_id": "root-query",
                "messages": [{"role": "user", "content": "review local changes"}]
            }))
            .expect("body should serialize"),
        );

        let prepared =
            prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
                .expect("explicit identity should prepare");

        assert_eq!(prepared.session_turn.as_deref(), Some("2"));
        assert_eq!(prepared.turn_chain_id.as_deref(), Some("root-chain"));
        assert_eq!(prepared.user_query_event_id.as_deref(), Some("root-query"));
    }

    #[tokio::test]
    async fn prepare_body_rejects_partial_explicit_turn_identity() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "session_turn": 2,
                "messages": [{"role": "user", "content": "review local changes"}]
            }))
            .expect("body should serialize"),
        );

        let error =
            match prepare_chat_turn_bridge_body(&state, &test_user(), body, Some("bound-session"))
                .await
            {
                Ok(_) => panic!("partial identity should be rejected"),
                Err(error) => error,
            };
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn prepare_body_propagates_full_capture_flag_from_session_metadata() {
        let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_session_service(Arc::new(CaptureEnabledSessionService));
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "selected_model": selected_model(),
                "session_id": "capture-session",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .expect("body should serialize"),
        );

        let prepared = prepare_chat_turn_bridge_body(&state, &test_user(), body, None)
            .await
            .expect("session metadata should be available");

        assert_eq!(
            prepared.trusted_session_id.as_deref(),
            Some("capture-session")
        );
        assert_eq!(prepared.full_llm_capture, Some(true));
    }

    // ── same_tool_names ─────────────────────────────────────────────

    #[test]
    fn same_tool_names_compares_sets_not_order_or_count() {
        for (left, right, expected) in [
            (vec![], vec![], true),
            (vec!["bash"], vec!["bash"], true),
            (vec!["bash", "grep"], vec!["grep", "bash"], true),
            (vec!["bash", "bash"], vec!["bash"], true),
            (vec!["bash"], vec!["grep"], false),
            (vec!["bash", "grep"], vec!["bash"], false),
        ] {
            let left_tools = left.into_iter().map(tool_value).collect::<Vec<_>>();
            let right_tools = right.into_iter().map(tool_value).collect::<Vec<_>>();
            assert_eq!(
                same_tool_names(&left_tools, &right_tools),
                expected,
                "left={left_tools:?} right={right_tools:?}"
            );
        }
        for (left, right) in [
            (vec![json!({})], vec![json!({})]),
            (
                vec![json!({}), tool_value("bash")],
                vec![tool_value("bash")],
            ),
            (
                vec![json!({ "function": {} })],
                vec![json!({ "function": {} })],
            ),
        ] {
            assert!(
                same_tool_names(&left, &right),
                "unnamed schemas must be ignored: left={left:?} right={right:?}"
            );
        }
    }

    // ── sync_cached_bridge_field ────────────────────────────────────

    #[test]
    fn sync_cached_bridge_field_prefers_incoming_else_cached_non_null_value() {
        let complex = json!({
            "rules": [{"id": 1, "text": "do this"}, {"id": 2, "text": "do that"}],
            "meta": {"version": 3}
        });
        for (cached, incoming, expected) in [
            (None, Some(json!("rule-value")), Some(json!("rule-value"))),
            (
                Some(json!("cached-value")),
                None,
                Some(json!("cached-value")),
            ),
            (
                Some(json!("old-cached")),
                Some(json!("new-incoming")),
                Some(json!("new-incoming")),
            ),
            (
                Some(json!("cached-value")),
                Some(serde_json::Value::Null),
                Some(json!("cached-value")),
            ),
            (None, None, None),
            (None, Some(complex.clone()), Some(complex.clone())),
        ] {
            let mut entry = serde_json::Map::new();
            if let Some(value) = cached {
                entry.insert("project_rules".to_string(), value);
            }
            let mut object = serde_json::Map::new();
            if let Some(value) = incoming {
                object.insert("project_rules".to_string(), value);
            }

            sync_cached_bridge_field(&mut entry, &mut object, "project_rules");

            assert_eq!(entry.get("project_rules"), expected.as_ref());
            assert_eq!(object.get("project_rules"), expected.as_ref());
        }
    }

    // ── inject_bridge_cache_state ───────────────────────────────────

    #[test]
    fn inject_bridge_cache_state_contract() {
        let mut empty_object = serde_json::Map::new();
        inject_bridge_cache_state(&serde_json::Map::new(), &mut empty_object);
        assert!(empty_object.get("bridge_cache_state").is_none());

        let mut entry = serde_json::Map::new();
        entry.insert("created_at".to_string(), json!("2024-01-15T10:30:00Z"));
        entry.insert("history".to_string(), json!(["turn1", "turn2"]));
        let mut object = serde_json::Map::new();
        object.insert("bridge_cache_state".to_string(), json!("old-stuff"));

        inject_bridge_cache_state(&entry, &mut object);

        let state = object
            .get("bridge_cache_state")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            state.get("created_at"),
            Some(&json!("2024-01-15T10:30:00Z"))
        );
        assert_eq!(state.get("history"), Some(&json!(["turn1", "turn2"])));
        assert_eq!(state.get("turn_count"), Some(&json!(0)));
    }

    // ── trim_edge_tools_for_result_turn ─────────────────────────────

    /// Helper to build a ChatTurnRequestBody from a Map for testing.
    fn request_from_map(map: serde_json::Map<String, serde_json::Value>) -> ChatTurnRequestBody {
        serde_json::from_value(serde_json::Value::Object(map)).unwrap_or_default()
    }

    #[test]
    fn trim_edge_tools_for_result_turn_contract() {
        let mut empty = ChatTurnRequestBody::default();
        trim_edge_tools_for_result_turn(&mut empty, "some query");
        assert!(empty.edge_tools_vec().is_none());

        for (tool_results, edge_tools, user_query, expected_names) in [
            (None, Some(vec![tool_value("bash")]), "", Some(vec!["bash"])),
            (
                Some(json!([{"name": "bash", "output": "ok"}])),
                None,
                "",
                None,
            ),
            (
                Some(json!([{"name": "bash", "output": "ok"}])),
                Some(vec![tool_value("bash"), tool_value("grep")]),
                "what happened?",
                Some(vec!["bash", "grep"]),
            ),
            (
                Some(json!([{"name": "bash", "output": "ok"}])),
                Some(vec![
                    tool_value("bash"),
                    tool_value("grep"),
                    tool_value("view"),
                ]),
                "",
                Some(vec!["bash"]),
            ),
            (
                Some(json!([])),
                Some(vec![tool_value("bash"), tool_value("grep")]),
                "",
                Some(vec!["bash", "grep"]),
            ),
        ] {
            let mut map = serde_json::Map::new();
            if let Some(results) = tool_results {
                map.insert("tool_results".to_string(), results);
            }
            if let Some(tools) = edge_tools {
                map.insert("edge_tools".to_string(), serde_json::Value::Array(tools));
            }
            let mut request = request_from_map(map);

            trim_edge_tools_for_result_turn(&mut request, user_query);

            let actual_names = request.edge_tools_vec().map(|tools| {
                tools
                    .iter()
                    .filter_map(extract_tool_name)
                    .collect::<Vec<_>>()
            });
            assert_eq!(actual_names, expected_names);
        }
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
            "selected_model": {"model": "gpt-4", "gateway": null},
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
        assert_eq!(request.selected_model_name(), Some("gpt-4"));
        assert!(serialized.get("model").is_none());
        assert!(request.execution_state_obj().is_some());

        // Forward-compat: unknown fields preserved
        assert_eq!(serialized.get("custom_field").unwrap(), &json!("preserved"));
    }

    #[test]
    fn session_id_shape_validation_contract() {
        for (raw, expected) in [
            (r#"{"messages":[]}"#, None),
            (r#"{"session_id":null}"#, None),
            (r#"{"session_id":"sess-1"}"#, None),
            (r#"{"session_id":42}"#, Some("session_id must be a string")),
            (r#"{"session_id":""}"#, Some("session_id must not be empty")),
            (
                r#"{"session_id":"   "}"#,
                Some("session_id must not be empty"),
            ),
        ] {
            let request: ChatTurnRequestBody = serde_json::from_str(raw).unwrap();
            match expected {
                Some(message) => {
                    let (status, body) = validate_session_id_shape(&request).unwrap_err();
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
                    assert_eq!(body.0.detail, message, "{raw}");
                }
                None => validate_session_id_shape(&request).unwrap_or_else(|err| {
                    panic!("{raw} should be valid, got {:?}", err.1.0.detail)
                }),
            }
        }
    }

    #[test]
    fn selected_model_shape_validation_contract() {
        for (raw, expected_code) in [
            (r#"{}"#, Some("missing_model_selection")),
            (
                r#"{"selected_model":"gpt-4"}"#,
                Some("selected_model_invalid"),
            ),
            (r#"{"selected_model":{}}"#, Some("selected_model_invalid")),
            (
                r#"{"selected_model":{"model":""}}"#,
                Some("selected_model_invalid"),
            ),
            (
                r#"{"selected_model":{"model":" gpt-4"}}"#,
                Some("selected_model_invalid"),
            ),
            (
                "{\"selected_model\":{\"model\":\"gpt\\n4\"}}",
                Some("selected_model_invalid"),
            ),
            (
                r#"{"selected_model":{"model":"gpt-4","gateway":42}}"#,
                Some("selected_model_invalid"),
            ),
            (
                r#"{"selected_model":{"model":"gpt-4","provider":"openai"}}"#,
                Some("selected_model_invalid"),
            ),
            (
                r#"{"selected_model":{"model":"gpt-4"},"model":"gpt-4"}"#,
                Some("selected_model_invalid"),
            ),
            (r#"{"selected_model":{"model":"gpt-4"}}"#, None),
            (
                r#"{"selected_model":{"model":"gpt-4","gateway":null}}"#,
                None,
            ),
            (
                r#"{"selected_model":{"model":"gpt-4","gateway":"gw-1"}}"#,
                None,
            ),
        ] {
            let request: ChatTurnRequestBody = serde_json::from_str(raw).unwrap();
            match expected_code {
                Some(code) => {
                    let (status, body) = validate_selected_model_shape(&request).unwrap_err();
                    assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
                    assert_eq!(body.0.error_code.as_deref(), Some(code), "{raw}");
                }
                None => validate_selected_model_shape(&request).unwrap_or_else(|err| {
                    panic!("{raw} should be valid, got {:?}", err.1.0.detail)
                }),
            }
        }
    }

    #[test]
    fn typed_request_mutators_only_touch_their_fields() {
        let mut request = ChatTurnRequestBody::default();
        request.set_session_id("new-sess");
        assert_eq!(request.session_id_str(), Some("new-sess"));

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
    fn extract_tool_name_fail_closed_contract() {
        for (schema, expected) in [
            (tool_value("bash"), Some("bash")),
            (json!({"type": "custom"}), None),
            (json!({"function": {}}), None),
            (json!({"function": {"name": 42}}), None),
            (
                json!({"type": "custom", "function": {"name": "leaked"}}),
                None,
            ),
            (
                json!({"function": {"name": "shorthand"}}),
                Some("shorthand"),
            ),
        ] {
            assert_eq!(extract_tool_name(&schema), expected, "schema={schema}");
        }
    }

    // ── sync_opt_field_with_cache ───────────────────────────────────

    #[test]
    fn sync_opt_field_with_cache_contract() {
        for (cached, field_value, expected) in [
            (None, Some(json!("rule-value")), Some(json!("rule-value"))),
            (
                Some(json!("cached-value")),
                None,
                Some(json!("cached-value")),
            ),
            (
                Some(json!("cached-value")),
                Some(serde_json::Value::Null),
                Some(json!("cached-value")),
            ),
            (None, None, None),
        ] {
            let mut entry = serde_json::Map::new();
            if let Some(value) = cached {
                entry.insert("rules".to_string(), value);
            }
            let mut field = field_value;

            sync_opt_field_with_cache(&mut entry, "rules", &mut field);

            assert_eq!(entry.get("rules"), expected.as_ref());
            assert_eq!(field, expected);
        }
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
        assert!(result.session_turn.is_none());
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
