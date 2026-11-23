use super::super::*;
use astra_services::runs::transform_run_event_for_client;

pub(crate) fn transform_stream_run_events_for_client(
    run_id: &str,
    events: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    let mut pending_run_error: Option<String> = None;
    transform_stream_run_events_for_client_with_pending(run_id, events, &mut pending_run_error)
}

pub(crate) fn transform_stream_run_events_for_client_with_pending(
    run_id: &str,
    events: Vec<serde_json::Value>,
    pending_run_error: &mut Option<String>,
) -> Vec<serde_json::Value> {
    let mut transformed_events = Vec::with_capacity(events.len());

    for event in events {
        let index = event.get("index").cloned();
        let producer_run_id = event
            .get("run_id")
            .or_else(|| event.get("data").and_then(|data| data.get("run_id")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let event_type = event
            .get("event_type")
            .or_else(|| event.get("type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let run_finished_cancelled = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("cancelled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let run_finished_interrupted = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("interrupted"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let run_finished_owner_generation = event
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|data| data.get("owner_generation"))
            .cloned();
        let run_finished_has_failure_facts = event_type == "run_finished"
            && event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|data| {
                    data.get("error")
                        .and_then(serde_json::Value::as_str)
                        .is_some()
                        || data
                            .get("error_code")
                            .and_then(serde_json::Value::as_str)
                            .is_some()
                });

        let event_belongs_to_root = producer_run_id
            .as_deref()
            .is_none_or(|producer_run_id| producer_run_id == run_id);
        if event_type == "run_error" && event_belongs_to_root {
            *pending_run_error = event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .and_then(|data| data.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
        }

        // The shared physical stream may include descendant lifecycle events.
        // Only its bound root owns root-level token/context accounting; a
        // descendant's own physical stream still satisfies this predicate.
        if event_type == "run_finished" && event_belongs_to_root {
            let data = event
                .get("data")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();

            let has_token_usage = [
                "prompt_tokens",
                "completion_tokens",
                "cache_read_tokens",
                "cache_creation_tokens",
            ]
            .iter()
            .any(|key| data.contains_key(*key));

            let normalized = if let Some(qualified) = data.get("qualified_usage") {
                // C0 already owns qualification. Unknown buckets are not bad
                // numbers, and its aggregate must not be rebuilt from mirrors.
                if qualified.is_null() {
                    Some(serde_json::Map::from_iter([(
                        "qualified_usage".into(),
                        serde_json::Value::Null,
                    )]))
                } else {
                    match astra_turn_types::CanonicalTokenUsage::from_json(qualified) {
                        Ok(usage) => {
                            let canonical = usage.to_json();
                            let mut fields = canonical
                                .as_object()
                                .expect("canonical usage is an object")
                                .clone();
                            fields.insert("qualified_usage".into(), canonical);
                            Some(fields)
                        }
                        Err(_) => {
                            tracing::warn!(run_id, index = ?index,
                                "run terminal contained malformed qualified token usage; suppressing its external usage projection");
                            None
                        }
                    }
                }
            } else if has_token_usage {
                let read = |key: &str| match data.get(key) {
                    None => Some(0),
                    Some(value) => value.as_u64(),
                };
                match (
                    read("prompt_tokens"),
                    read("cache_read_tokens"),
                    read("cache_creation_tokens"),
                    read("completion_tokens"),
                ) {
                    (Some(input), Some(cached), Some(created), Some(output)) => {
                        Some(crate::turn::token_usage::TokenUsage {
                            input_tokens: input,
                            cached_input_tokens: cached,
                            cache_creation_tokens: created,
                            output_tokens: output,
                        })
                    }
                    _ => {
                        tracing::warn!(
                            run_id,
                            index = ?index,
                            "run terminal contained malformed token usage; suppressing its external usage projection"
                        );
                        None
                    }
                }.map(|usage| usage.to_json_map())
            } else {
                None
            };
            // Only the final receipt advances the durable-event cursor. Prefix
            // projections must remain replayable if publication stops between frames.
            if let Some(mut usage) = normalized {
                usage.insert(
                    "type".to_string(),
                    serde_json::Value::String("usage".to_string()),
                );
                usage.insert(
                    "usage_scope".to_string(),
                    serde_json::Value::String("run_total".to_string()),
                );
                for key in ["last_request_usage", "last_request_input_tokens"] {
                    if let Some(value) = data.get(key) {
                        usage.insert(key.to_string(), value.clone());
                    }
                }
                if let Some(tool_call_count) = data
                    .get("tool_call_count")
                    .and_then(serde_json::Value::as_u64)
                {
                    usage.insert(
                        "tool_call_count".to_string(),
                        serde_json::Value::from(tool_call_count),
                    );
                }
                transformed_events.push(serde_json::Value::Object(usage));
            }

            // `run_finished` records an accounting total.  A separate,
            // explicitly scoped event exposes the final physical request for
            // context-window and cache-rate UI without guessing from totals.
            let latest_request = data
                .get("last_request_usage")
                .and_then(serde_json::Value::as_object);
            if let Some(latest_request) = latest_request {
                let read = |key: &str| latest_request.get(key).and_then(serde_json::Value::as_u64);
                match (
                    read("prompt_tokens"),
                    read("cache_read_tokens"),
                    read("cache_creation_tokens"),
                    read("completion_tokens"),
                ) {
                    (
                        Some(input_tokens),
                        Some(cached_input_tokens),
                        Some(cache_creation_tokens),
                        Some(output_tokens),
                    ) => {
                        let mut context_usage = serde_json::Map::new();
                        context_usage.insert(
                            "type".to_string(),
                            serde_json::Value::String("context_usage".to_string()),
                        );
                        context_usage.insert(
                            "input_tokens".to_string(),
                            serde_json::Value::from(input_tokens),
                        );
                        context_usage.insert(
                            "cached_input_tokens".to_string(),
                            serde_json::Value::from(cached_input_tokens),
                        );
                        context_usage.insert(
                            "cache_creation_tokens".to_string(),
                            serde_json::Value::from(cache_creation_tokens),
                        );
                        context_usage.insert(
                            "output_tokens".to_string(),
                            serde_json::Value::from(output_tokens),
                        );
                        transformed_events.push(serde_json::Value::Object(context_usage));
                    }
                    _ => {
                        tracing::warn!(run_id, index = ?index, "run terminal contained malformed latest request usage; suppressing its context projection")
                    }
                }
            }
        }

        let mut transformed = transform_run_event_for_client(event);
        // wip-7 allowlist: the transform returns `Value::Null` for
        // events outside the external client allowlist (e.g.
        // `injection_freshness`). Skip them entirely rather than
        // pushing a bare `null` into the SSE stream — that would
        // either serialize as `data: null` (confusing) or need
        // downstream null-skipping everywhere.
        if transformed.is_null() {
            continue;
        }
        if should_inject_run_id(event_type.as_str())
            && let Some(obj) = transformed.as_object_mut()
            && !obj.contains_key("run_id")
        {
            obj.insert(
                "run_id".to_string(),
                serde_json::Value::String(
                    producer_run_id
                        .clone()
                        .unwrap_or_else(|| run_id.to_string()),
                ),
            );
        }
        if event_type == "run_finished"
            && event_belongs_to_root
            && let Some(obj) = transformed.as_object_mut()
        {
            if let Some(owner_generation) = run_finished_owner_generation {
                obj.insert("owner_generation".to_string(), owner_generation);
            }
            let status = if obj.get("status").is_some() {
                None
            } else if run_finished_cancelled {
                Some("cancelled")
            } else if run_finished_interrupted {
                Some("paused")
            } else if run_finished_has_failure_facts || pending_run_error.is_some() {
                Some("failed")
            } else {
                Some("completed")
            };

            if let Some(status) = status {
                obj.insert(
                    "status".to_string(),
                    serde_json::Value::String(status.to_string()),
                );
            }
            // An earlier diagnostic cannot overwrite the owner's selected
            // outcome or its cause. Retain the legacy missing-cause projection
            // only for a failing receipt; canonical receipts carry their cause.
            if let Some(error) = pending_run_error.take()
                && matches!(
                    obj.get("status").and_then(serde_json::Value::as_str),
                    Some("failed" | "cancelled")
                )
                && obj
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            {
                obj.insert("error".to_string(), serde_json::Value::String(error));
            }
        }

        if let Some(index) = index
            && let Some(obj) = transformed.as_object_mut()
        {
            obj.insert("index".to_string(), index);
        }
        transformed_events.push(transformed);
    }

    transformed_events
}

fn should_inject_run_id(event_type: &str) -> bool {
    matches!(
        event_type,
        "run_started"
            | "run_error"
            | "run_interrupted"
            | "run_waiting"
            | "run_paused"
            | "run_resumed"
            | "approval_required"
            | "runtime.control.handoff.requested"
            | "runtime.control.handoff.rejected"
            | "ask_user_prompted"
            | "user_prompt_required"
            | "provider_interaction_required"
            | "provider_interaction_resolved"
            | "user_intent"
            | "permission_mode_applied"
            | "run_model_outcome"
            | "user_intent_applied"
            | "user_intent_returned"
            | "run_finished"
    ) || event_type == "run_blocked"
}

pub(crate) async fn get_run_status_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let run = state
        .execution
        .run_lifecycle_service
        .get_run_status(run_id, user.user_id)
        .await?;
    Ok(Json(RunStatusResponse::from(run)))
}

/// Read the current durable approval frontier without loading the transcript.
pub(crate) async fn get_run_approval_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<Option<serde_json::Value>>, (StatusCode, Json<ErrorResponse>)> {
    let principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(
                &method,
                &uri,
                &headers,
                "/chat/runs/{run_id}/approval",
                &Bytes::new(),
            ),
        )
        .await?;
    authorize_provider_run_session(&state, &principal, &run_id).await?;
    let projection = state
        .execution
        .run_lifecycle_service
        .get_run_interaction_projection(
            run_id.clone(),
            principal.user.user_id,
            astra_services::runs::DurableRunInteractionKind::Approval,
        )
        .await?
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))?;
    if projection.status != STATUS_WAITING
        || projection.waiting_for.as_deref() != Some("tool_approval")
        || projection.recent_events.iter().any(|event| {
            event.get("event_type").and_then(serde_json::Value::as_str) == Some("approval_resolved")
        })
    {
        return Ok(Json(None));
    }
    let required = projection.recent_events.into_iter().find(|event| {
        event.get("event_type").and_then(serde_json::Value::as_str) == Some("approval_required")
    });
    let mut event = required.and_then(|event| {
        transform_stream_run_events_for_client(&run_id, vec![event])
            .into_iter()
            .next()
    });
    if let Some(object) = event.as_mut().and_then(serde_json::Value::as_object_mut) {
        object.remove("scoped_permission_snapshot");
    }
    Ok(Json(event))
}

#[derive(serde::Deserialize)]
pub(crate) struct RunProjectionQuery {
    #[serde(default = "default_projection_recent_limit")]
    pub recent_limit: u32,
}

fn default_projection_recent_limit() -> u32 {
    20
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionCheckpointResponse {
    pub checkpoint_id: String,
    pub checkpoint_kind: String,
    pub checkpoint_version: String,
    pub node_seq: i64,
    pub created_at: String,
}

#[derive(serde::Serialize)]
pub(crate) struct RunProjectionResponse {
    pub run_id: String,
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_origin: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    pub run_event_high_watermark: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_event_type: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// Last persisted aggregate; live execution has no exact count here.
    pub total_tool_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_checkpoint: Option<RunProjectionCheckpointResponse>,
    pub recent_events: Vec<serde_json::Value>,
}

impl RunProjectionResponse {
    fn new(mut value: astra_services::runs::RunProjectionRecord) -> Self {
        value.recent_events =
            transform_stream_run_events_for_client(&value.run_id, value.recent_events);
        Self {
            run_id: value.run_id,
            session_id: value.session_id,
            status: value.status,
            cancellation_origin: value.cancellation_origin.map(|origin| origin.as_str()),
            waiting_for: value.waiting_for,
            error_message: value.error_message,
            workspace: value.workspace,
            executor: value.executor,
            transport: value.transport,
            run_event_high_watermark: value.run_event_high_watermark,
            latest_event_type: value.latest_event_type,
            total_prompt_tokens: value.total_prompt_tokens,
            total_completion_tokens: value.total_completion_tokens,
            total_tool_calls: value.total_tool_calls,
            latest_checkpoint: value.latest_checkpoint.map(|checkpoint| {
                RunProjectionCheckpointResponse {
                    checkpoint_id: checkpoint.checkpoint_id,
                    checkpoint_kind: checkpoint.checkpoint_kind,
                    checkpoint_version: checkpoint.checkpoint_version,
                    node_seq: checkpoint.node_seq,
                    created_at: checkpoint.created_at,
                }
            }),
            recent_events: value.recent_events,
        }
    }
}

pub(crate) async fn get_run_projection_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunProjectionQuery>,
) -> Result<Json<RunProjectionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let projection = state
        .execution
        .run_lifecycle_service
        .get_run_projection(run_id, user.user_id, query.recent_limit)
        .await?;
    Ok(Json(RunProjectionResponse::new(projection)))
}

pub(crate) async fn authorize_provider_run_session(
    state: &AppState,
    principal: &AuthPrincipal,
    run_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let AuthPrincipalOrigin::ProviderAuthorizedRequest(context) = &principal.origin else {
        return Ok(());
    };
    let owner = astra_services::runs::ProviderRunOwner {
        provider_id: context.provider_id.clone(),
        provider_scope_id: context.provider_scope_id.clone(),
    };
    state
        .execution
        .run_lifecycle_service
        .authorize_provider_run_scope(principal.user.user_id.clone(), run_id.to_owned(), owner)
        .await
}

pub(crate) async fn stream_run_handler(
    State(state): State<AppState>,
    trace: Option<axum::Extension<crate::server::RequestTrace>>,
    Path(run_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Query(query): Query<RunStreamQuery>,
) -> Response {
    let request_id = trace
        .as_ref()
        .map(|axum::Extension(trace)| trace.request_id.clone());
    let principal = match state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(
                &method,
                &uri,
                &headers,
                "/chat/runs/{run_id}/stream",
                &Bytes::new(),
            ),
        )
        .await
    {
        Ok(principal) => principal,
        Err((status, error)) => {
            return sse_error_response_from_error_with_context(
                status,
                error.0,
                SseErrorContext {
                    request_id: request_id.as_deref(),
                    run_id: Some(&run_id),
                    ..SseErrorContext::default()
                },
            );
        }
    };

    if let Err((status, error)) = authorize_provider_run_session(&state, &principal, &run_id).await
    {
        return sse_error_response_from_error_with_context(
            status,
            error.0,
            SseErrorContext {
                request_id: request_id.as_deref(),
                run_id: Some(&run_id),
                ..SseErrorContext::default()
            },
        );
    }

    if query.replay_only {
        use crate::server::http_helpers::{
            sse_error_event, sse_event_frame, sse_run_response_body,
        };

        let service = state.execution.run_lifecycle_service.clone();
        let user_id = principal.user.user_id;
        let first = match service
            .stream_run(
                run_id.clone(),
                user_id.clone(),
                i64::from(query.last_index),
                i64::MAX,
            )
            .await
        {
            Ok(first) => first,
            Err((status, error)) => {
                return sse_error_response_from_error_with_context(
                    status,
                    error.0,
                    SseErrorContext {
                        request_id: request_id.as_deref(),
                        run_id: Some(&run_id),
                        ..SseErrorContext::default()
                    },
                );
            }
        };

        let stream = async_stream::stream! {
            let ceiling = first.last_event_idx;
            let session_id = first.session_id.clone();
            let context = SseErrorContext {
                request_id: request_id.as_deref(),
                run_id: Some(&run_id),
                session_id: Some(&session_id),
            };
            let mut pending_run_error = None;
            let mut page = first;
            loop {
                for event in transform_stream_run_events_for_client_with_pending(
                    &run_id, page.events, &mut pending_run_error,
                ) {
                    yield Ok::<_, std::convert::Infallible>(sse_event_frame(&event));
                }
                let cursor = page.through_event_idx;
                if cursor >= ceiling {
                    yield Ok("data: [DONE]\n\n".to_owned());
                    break;
                }
                let next = service.stream_run(
                    run_id.clone(), user_id.clone(), cursor.saturating_add(1), ceiling,
                ).await;
                match next {
                    Ok(next) if next.session_id == session_id && next.through_event_idx > cursor => {
                        page = next;
                    }
                    failure => {
                        let (status, error) = failure.err().unwrap_or_else(|| {
                            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Run replay could not continue")
                        });
                        yield Ok(sse_event_frame(&sse_error_event(status, error.0, context)));
                        break;
                    }
                }
            }
        };
        return sse_run_response_body(Body::from_stream(stream));
    }

    match state
        .execution
        .run_lifecycle_service
        .stream_run_live(
            run_id.clone(),
            principal.user.user_id,
            query.last_index,
            false,
        )
        .await
    {
        Ok(mut stream) => {
            if let Some(event_rx) = stream.event_rx.take() {
                crate::server::http_helpers::sse_run_stream_response(
                    stream.session_id,
                    stream.run_id,
                    request_id.clone(),
                    event_rx,
                    stream.attachment_completion,
                )
            } else {
                let mut events = vec![serde_json::json!({
                    "type":"session_info", "session_id":stream.session_id, "run_id":stream.run_id,
                })];
                events.extend(transform_stream_run_events_for_client(
                    &run_id,
                    stream.events,
                ));
                sse_json_response(events)
            }
        }
        Err((status, error)) => sse_error_response_from_error_with_context(
            status,
            error.0,
            SseErrorContext {
                request_id: request_id.as_deref(),
                run_id: Some(&run_id),
                ..SseErrorContext::default()
            },
        ),
    }
}

pub(crate) async fn get_run_execution_settlement_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(
                &method,
                &uri,
                &headers,
                "/chat/runs/{run_id}/cancel",
                &Bytes::new(),
            ),
        )
        .await?;
    authorize_provider_run_session(&state, &principal, &run_id).await?;
    let observed = state
        .execution
        .run_lifecycle_service
        .get_run_execution_settlement(run_id, principal.user.user_id)
        .await
        .map_err(normalize_cancel_run_error)?;
    Ok(Json(CancelRunResponse::from(observed)))
}

pub(crate) async fn cancel_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    cancel_run_for_user(&state, run_id, user.user_id).await
}

pub(crate) async fn delete_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/chat/runs/{run_id}", &body),
        )
        .await?;
    authorize_provider_run_session(&state, &principal, &run_id).await?;
    cancel_run_for_user(&state, run_id, principal.user.user_id).await
}

async fn cancel_run_for_user(
    state: &AppState,
    run_id: String,
    user_id: String,
) -> Result<Json<CancelRunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let result = state
        .execution
        .run_lifecycle_service
        .cancel_run(run_id, user_id)
        .await
        .map_err(normalize_cancel_run_error)?;
    Ok(Json(CancelRunResponse::from(result)))
}

fn normalize_cancel_run_error(
    error: (StatusCode, Json<ErrorResponse>),
) -> (StatusCode, Json<ErrorResponse>) {
    if error.0 == StatusCode::NOT_FOUND {
        error_response_coded(StatusCode::NOT_FOUND, "Run not found", "run_not_found")
    } else {
        error
    }
}

pub(crate) async fn list_runs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RunListQuery>,
) -> Result<Json<RunListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let cursor = query.cursor()?;
    let runs = state
        .execution
        .run_lifecycle_service
        .list_runs_cursor(user.user_id, query.limit, cursor)
        .await?;
    Ok(Json(RunListResponse::from(runs)))
}

pub(crate) async fn pause_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .pause_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
}

pub(crate) async fn resume_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RunMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .resume_run(run_id, user.user_id)
        .await?;
    Ok(Json(RunMutationResponse::from(result)))
}

#[derive(serde::Deserialize)]
pub(crate) struct RunUserIntentRequest {
    pub intent_id: String,
    pub delivery: astra_turn_types::UserIntentDelivery,
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(serde::Serialize)]
pub(crate) struct RunUserIntentResponse {
    pub run_id: String,
    pub intent_id: String,
    pub status: astra_turn_types::UserIntentStatus,
    pub duplicate: bool,
    pub event_index: i64,
}

pub(crate) async fn submit_run_user_intent_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RunUserIntentRequest>,
) -> Result<Json<RunUserIntentResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .execution
        .run_lifecycle_service
        .submit_run_user_intent(
            run_id,
            user.user_id,
            astra_services::runs::RunUserIntentData {
                intent_id: request.intent_id,
                delivery: request.delivery,
                input: request.input,
            },
        )
        .await?;
    Ok(Json(RunUserIntentResponse {
        run_id: result.run_id,
        intent_id: result.intent_id,
        status: result.status,
        duplicate: result.duplicate,
        event_index: result.event_index,
    }))
}

pub(crate) async fn run_user_intent_status_handler(
    State(state): State<AppState>,
    Path((run_id, intent_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<RunPermissionModeQuery>,
) -> Result<Json<astra_turn_types::UserIntentStatus>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .execution
        .run_lifecycle_service
        .run_user_intent_status(user.user_id, query.expected_session_id, run_id, intent_id)
        .await
        .map(Json)
}

#[derive(serde::Deserialize)]
pub(crate) struct RunPermissionModeQuery {
    pub expected_session_id: String,
}

pub(crate) async fn request_run_model_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<astra_turn_types::RunModelRequest>,
) -> Result<Json<astra_turn_types::RunModelSelection>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .execution
        .run_lifecycle_service
        .request_run_model(user.user_id, run_id, request)
        .await
        .map(Json)
}

pub(crate) async fn run_model_snapshot_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunPermissionModeQuery>,
) -> Result<Json<astra_turn_types::RunModelSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .execution
        .run_lifecycle_service
        .run_model_snapshot(user.user_id, query.expected_session_id, run_id)
        .await
        .map(Json)
}

pub(crate) async fn request_permission_mode_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<astra_turn_types::RunPermissionModeRequest>,
) -> Result<Json<astra_turn_types::RunPermissionModeSelection>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .execution
        .run_lifecycle_service
        .request_permission_mode(user.user_id, run_id, request)
        .await
        .map(Json)
}

pub(crate) async fn permission_mode_snapshot_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RunPermissionModeQuery>,
) -> Result<Json<astra_turn_types::RunPermissionModeSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .execution
        .run_lifecycle_service
        .permission_mode_snapshot(user.user_id, query.expected_session_id, run_id)
        .await
        .map(Json)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[test]
    fn cancel_run_not_found_has_a_stable_machine_code() {
        let normalized = normalize_cancel_run_error(error_response(
            StatusCode::NOT_FOUND,
            "private lookup detail",
        ));
        assert_eq!(normalized.0, StatusCode::NOT_FOUND);
        assert_eq!(normalized.1.0.detail, "Run not found");
        assert_eq!(normalized.1.0.error_code.as_deref(), Some("run_not_found"));
    }

    #[test]
    fn cancel_run_non_not_found_error_is_unchanged() {
        let original = error_response_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage unavailable",
            "storage_unavailable",
        );
        let normalized = normalize_cancel_run_error(original);
        assert_eq!(normalized.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(normalized.1.0.detail, "storage unavailable");
        assert_eq!(
            normalized.1.0.error_code.as_deref(),
            Some("storage_unavailable")
        );
    }
    use async_trait::async_trait;
    use axum::{
        Json,
        body::{self, Body},
        extract::Query,
        http::{HeaderMap, Request, StatusCode, Uri},
    };
    use serde_json::json;
    use tokio::sync::Mutex as TokioMutex;
    use tower::util::ServiceExt;

    use crate::{
        AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData,
        AuthService, AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
        build_app,
    };

    static HTTP_RUN_DB: tokio::sync::OnceCell<astra_core::SharedPool> =
        tokio::sync::OnceCell::const_new();

    #[derive(Clone)]
    struct StubHealthChecker;

    #[async_trait]
    impl HealthChecker for StubHealthChecker {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Clone)]
    struct StubAuthService;

    #[async_trait]
    impl AuthService for StubAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            if headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer good-token")
            {
                Ok(AuthUserRecord {
                    user_id: "u1".to_string(),
                    username: "test-user".to_string(),
                    email: "u1@example.test".to_string(),
                    display_name: None,
                })
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse::new("Not authenticated".to_string())),
                ))
            }
        }
    }

    #[derive(Clone)]
    struct ProviderRequestOnlyAuthService {
        descriptor: Arc<Mutex<Option<astra_services::ProviderRequestDescriptor>>>,
        scope: Option<&'static str>,
    }

    #[async_trait]
    impl AuthService for ProviderRequestOnlyAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn login(
            &self,
            _request: AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn current_user(
            &self,
            _headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "JWT auth must not handle provider cancellation".to_string(),
                )),
            ))
        }

        async fn current_principal_for_request(
            &self,
            _headers: &HeaderMap,
            request: astra_services::ProviderRequestDescriptor,
        ) -> Result<crate::AuthPrincipal, (StatusCode, Json<ErrorResponse>)> {
            *self.descriptor.lock().expect("descriptor lock") = Some(request);
            let user = AuthUserRecord {
                user_id: "provider-user-1".to_string(),
                username: "provider-user-1".to_string(),
                email: String::new(),
                display_name: None,
            };
            Ok(match self.scope {
                Some(scope) => crate::AuthPrincipal {
                    user,
                    session_id: None,
                    execution_continuation: None,
                    origin: astra_services::AuthPrincipalOrigin::ProviderAuthorizedRequest(
                        astra_services::AuthProviderAuthorizedRequestContext {
                            provider_id: "moi".into(),
                            external_subject: "provider-user-1".into(),
                            provider_scope_id: scope.into(),
                            request_authorization_id: "test-request".into(),
                            edge_agent_id: None,
                        },
                    ),
                },
                None => crate::AuthPrincipal::internal(user),
            })
        }
    }

    fn test_matrixone() -> astra_core::MatrixOneSettings {
        astra_core::MatrixOneSettings::mock()
    }

    #[tokio::test]
    async fn approval_read_uses_request_auth_and_fails_closed_without_provider_scope() {
        use crate::server::run::engine::{RunEngine, RunStartContext};
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::{
            AtomicRunInteractionBatchRegistrationRequest, AtomicRunInteractionWaitRequest,
            DurableRunInteractionKind, InMemoryRunStateStore, ProviderRunOwner,
        };

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        let mut context = RunStartContext::default();
        context.provider_run_owner = Some(ProviderRunOwner {
            provider_id: "moi".into(),
            provider_scope_id: "workspace-a".into(),
        });
        engine
            .start_run_with_context("run-approval", "provider-user-1", "session-1", context)
            .await
            .unwrap();
        let event = json!({"event_type":"approval_required", "idempotency_key":"approval-1",
            "data":{"request_id":"approval-1", "session_id":"session-1", "tool":"bash",
                "detail":"private action", "approval_kind":"protected_bash_write"}});
        engine
            .register_guarded_interaction_batch(AtomicRunInteractionBatchRegistrationRequest {
                user_id: "provider-user-1",
                run_id: "run-approval",
                expected_session_id: "session-1",
                expected_control_epoch: 0,
                expected_owner_generation: 0,
                events: &[event],
            })
            .await
            .unwrap();
        engine
            .begin_run_interaction_wait(AtomicRunInteractionWaitRequest {
                user_id: "provider-user-1",
                run_id: "run-approval",
                expected_session_id: "session-1",
                request_id: "approval-1",
                kind: DurableRunInteractionKind::Approval,
                expected_control_epoch: 0,
                expected_owner_generation: 0,
            })
            .await
            .unwrap();
        let lifecycle = Arc::new(AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(crate::FernetTokenEncryptor::new("0123456789abcdef").unwrap()),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        ));
        for (scope, expected) in [
            (None, StatusCode::OK),
            (Some("workspace-b"), StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let descriptor = Arc::new(Mutex::new(None));
            let app = build_app(
                AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                    .with_auth_service(Arc::new(ProviderRequestOnlyAuthService {
                        descriptor: Arc::clone(&descriptor),
                        scope,
                    }))
                    .with_run_lifecycle_service(lifecycle.clone()),
            );
            let response = app
                .oneshot(
                    Request::builder()
                        .uri("/chat/runs/run-approval/approval")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            let bytes = body::to_bytes(response.into_body(), 65536).await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&bytes).contains("private action"),
                scope.is_none()
            );
            let request = descriptor.lock().unwrap().clone().unwrap();
            assert_eq!(
                request.route.as_deref(),
                Some("/chat/runs/{run_id}/approval")
            );
        }
    }

    #[tokio::test]
    async fn delete_run_accepts_provider_request_principal() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run(
                "run-provider-cancel",
                "provider-user-1",
                "session-provider-cancel",
            )
            .await
            .expect("start provider-owned run");
        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );
        let descriptor = Arc::new(Mutex::new(None));
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(ProviderRequestOnlyAuthService {
                    descriptor: Arc::clone(&descriptor),
                    scope: None,
                }))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/chat/runs/run-provider-cancel")
                    .header("authorization", "Bearer provider-token")
                    .header("x-request-id", "provider-cancel-request")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body::to_bytes(response.into_body(), 1024)
            .await
            .expect("body should be readable");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("cancel response JSON");
        assert_eq!(payload["run_id"], "run-provider-cancel");
        assert_eq!(payload["status"], "cancelled");
        let descriptor = descriptor
            .lock()
            .expect("descriptor lock")
            .clone()
            .expect("provider request descriptor");
        assert_eq!(descriptor.method, "DELETE");
        assert_eq!(descriptor.path, "/chat/runs/run-provider-cancel");
        assert_eq!(descriptor.route.as_deref(), Some("/chat/runs/{run_id}"));
        assert_eq!(
            descriptor.request_id.as_deref(),
            Some("provider-cancel-request")
        );
        assert_eq!(
            descriptor.body_digest.as_deref(),
            Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-provider-cancel/cancel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "settlement observation must use request-aware authentication too"
        );
    }

    async fn setup_http_run_db_it() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        HTTP_RUN_DB
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                astra_services::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                astra_core::SharedPool::new(&settings)
                    .await
                    .expect("SharedPool::new")
            })
            .await
            .clone()
    }

    async fn cleanup_run_http_fixture(pool: &astra_core::SharedPool, user_id: &str, run_id: &str) {
        for sql in [
            "DELETE FROM run_checkpoints WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_run_openings WHERE user_id = ? AND run_id = ?",
            "DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?",
        ] {
            sqlx::query(sql)
                .bind(user_id)
                .bind(run_id)
                .execute(pool.get())
                .await
                .expect("cleanup run HTTP fixture");
        }
    }

    #[test]
    fn transform_stream_run_events_for_client_uses_client_protocol_shape() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "text_delta",
                    "data": {"chunk": "hello"},
                    "index": 7
                }),
                json!({
                    "event_type": "run_error",
                    "data": {"error": "boom"},
                    "index": 8
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "text_delta", "content": "hello", "index": 7})
        );
        assert_eq!(
            transformed[1],
            json!({"type": "run_error", "message": "boom", "error": "boom", "code": "RUN_ERROR", "run_id": "run-123", "index": 8})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_preserves_terminal_handoff_and_delegated_status() {
        let transformed = transform_stream_run_events_for_client(
            "run-source",
            vec![
                json!({
                    "type": "runtime.control.handoff.requested",
                    "handoff_id": "handoff-1",
                    "kind": "moi.control.handoff.v1",
                    "target": "agent_authoring",
                    "action": "revise_current_agent",
                    "terminal": true
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {
                        "status": "delegated",
                        "outcome": "delegated",
                        "prompt_tokens": 9,
                        "completion_tokens": 2
                    }
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "runtime.control.handoff.requested",
                "handoff_id": "handoff-1",
                "kind": "moi.control.handoff.v1",
                "target": "agent_authoring",
                "action": "revise_current_agent",
                "terminal": true,
                "run_id": "run-source"
            })
        );
        assert_eq!(transformed[1]["type"], "usage");
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_finished",
                "run_id": "run-source",
                "status": "delegated",
                "outcome": "delegated"
            })
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_maps_tool_result_to_tool_call_end() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "tool_result",
                "data": {"call_id": "call-1", "result": "ok"},
                "index": 9
            })],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok", "index": 9})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_projects_live_work_board_updates() {
        // This is the actual HTTP/SSE boundary, not just the services
        // projector.  A canonical Work receipt is emitted as a raw live
        // event, whereas reconnect replay uses the durable event envelope.
        // Both must reach interactive clients as the same small public shape.
        let update = json!({
            "schema_version": 1,
            "work_id": "work-1",
            "branch_id": "branch-1",
            "kind": "snapshot",
            "goal": "Inspect two files",
            "graph_revision": 2,
            "criteria_member_count": 0,
            "tasks": []
        });

        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "type": "work_task_board_update",
                "session_id": "session-1",
                "tool_call_id": "call-1",
                "task_board_update": update,
                "internal_diagnostic": "must not cross the SSE boundary",
                "index": 9,
            })],
        );

        assert_eq!(
            transformed,
            vec![json!({
                "type": "work_task_board_update",
                "session_id": "session-1",
                "tool_call_id": "call-1",
                "task_board_update": update,
                "index": 9,
            })]
        );
    }

    #[test]
    fn run_list_query_rejects_legacy_offset_param() {
        let legacy_uri: Uri = "/runs?limit=10&offset=5".parse().unwrap();
        assert!(
            Query::<RunListQuery>::try_from_uri(&legacy_uri).is_err(),
            "legacy run-list offset query must fail instead of silently returning page one"
        );

        let cursor_uri: Uri =
            "/runs?limit=10&after_updated_at=2024-01-02T00:00:00.000000&after_run_id=run-5"
                .parse()
                .unwrap();
        let Query(query) =
            Query::<RunListQuery>::try_from_uri(&cursor_uri).expect("cursor query should decode");
        assert!(query.cursor().unwrap().is_some());
    }

    #[test]
    fn run_stream_query_decodes_replay_only_flag() {
        let uri: Uri = "/chat/runs/run-1/stream?last_index=7&replay_only=true"
            .parse()
            .unwrap();
        let Query(query) = Query::<astra_server_types::RunStreamQuery>::try_from_uri(&uri)
            .expect("replay-only stream query should decode");
        assert_eq!(query.last_index, 7);
        assert!(query.replay_only);
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_usage_and_terminal_status() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_error",
                    "data": {"error": "boom", "error_code": "network", "error_kind": "network"},
                    "index": 10
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {
                        "prompt_tokens": 7,
                        "completion_tokens": 3,
                        "tool_call_count": 2,
                        "error_code": "network",
                        "error_kind": "network"
                    },
                    "index": 11
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {"cancelled": true, "prompt_tokens": 1, "completion_tokens": 0},
                    "index": 12
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "run_error",
                "message": "boom",
                "error": "boom",
                "code": "LLM_TRANSPORT_ERROR",
                "error_code": "network",
                "error_kind": "network",
                "retryable": true,
                "retry_after_ms": 3000,
                "run_id": "run-123",
                "index": 10
            })
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "usage",
                "input_tokens": 7,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 3,
                "total_tokens": 10,
                "usage_scope": "run_total",
                "tool_call_count": 2
            })
        );
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_finished",
                "run_id": "run-123",
                "status": "failed",
                "error": "boom",
                "error_code": "network",
                "error_kind": "network",
                "index": 11
            })
        );
        assert_eq!(
            transformed[3],
            json!({
                "type": "usage",
                "input_tokens": 1,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 1,
                "usage_scope": "run_total"
            })
        );
        assert_eq!(
            transformed[4],
            json!({"type": "run_finished", "run_id": "run-123", "status": "cancelled", "index": 12})
        );
    }

    #[test]
    fn failed_terminal_replay_does_not_require_the_preceding_run_error() {
        // Accounting prefixes cannot advance the cursor before the receipt.
        // A replay tail still carries the owner's cause without the diagnostic.
        let projected = transform_stream_run_events_for_client(
            "run-failed",
            vec![json!({
                "event_type": "run_finished",
                "index": 11,
                "data": {
                    "error": "boom",
                    "error_code": "network",
                    "owner_generation": 7,
                    "prompt_tokens": 2,
                    "completion_tokens": 1
                }
            })],
        );
        assert_eq!(projected[0]["type"], "usage");
        assert!(projected[0].get("index").is_none());
        assert_eq!(
            projected[1],
            json!({
                "type": "run_finished",
                "run_id": "run-failed",
                "index": 11,
                "status": "failed",
                "error": "boom",
                "error_code": "network",
                "owner_generation": 7
            })
        );
    }

    #[test]
    fn provisional_diagnostic_does_not_replace_the_selected_owner_cause() {
        for (status, cause) in [("completed", None), ("failed", Some("owner cause"))] {
            let projected = transform_stream_run_events_for_client(
                "root",
                vec![
                    json!({"event_type":"run_error","run_id":"root",
                    "data":{"error":"earlier attempt","error_kind":"database_error"}}),
                    json!({"event_type":"run_finished","run_id":"root",
                    "data":{"status":status,"owner_generation":4,"error":cause}}),
                ],
            );
            let receipt = projected.last().unwrap();
            assert_eq!(receipt["status"], status);
            assert_eq!(receipt["error"], json!(cause));
        }
    }

    #[test]
    fn canonical_terminal_usage_reaches_the_actual_sse_consumer_without_guessing() {
        use astra_services::runs::{
            CanonicalRunTerminalReceipt, CanonicalTerminalContext, CanonicalTerminalOutcome,
            CanonicalTerminalTranscript,
        };
        use astra_turn_types::CanonicalTokenUsage;
        let known = CanonicalTokenUsage::new(Some(7), Some(11), Some(2), Some(3)).unwrap();
        let samples = [
            Some(CanonicalTokenUsage::new(Some(13), None, None, Some(5)).unwrap()),
            Some(known),
            Some(CanonicalTokenUsage::new(Some(0), Some(0), Some(0), Some(0)).unwrap()),
            Some(CanonicalTokenUsage::new(None, None, None, None).unwrap()),
            None,
        ];
        for failed in [false, true] {
            for sample in samples {
                let error = failed.then_some("stream stopped");
                let receipt = CanonicalRunTerminalReceipt {
                    schema_version: 1,
                    user_id: "owner".into(),
                    session_id: "session".into(),
                    run_id: "run-usage-boundary".into(),
                    generation: 1,
                    opening_hash: "a".repeat(64),
                    context_admission_hash: None,
                    outcome: if failed {
                        CanonicalTerminalOutcome::Failed
                    } else {
                        CanonicalTerminalOutcome::Completed
                    },
                    error_hash: CanonicalRunTerminalReceipt::error_hash(error),
                    error_code: failed.then(|| "stream_transport".into()),
                    error_kind: failed.then_some(astra_core::ErrorKind::StreamTransport),
                    error_metadata: None,
                    interruption: None,
                    context: CanonicalTerminalContext::Unowned,
                    transcript: CanonicalTerminalTranscript {
                        item_count: 0,
                        digest: "b".repeat(64),
                        assistant_source_event_id: None,
                    },
                    usage: sample,
                    last_request_context: None,
                    tool_ledger: None,
                };
                let completion = receipt
                    .completion_events(None, None, error, Some(1), -1, 2)
                    .unwrap();
                let projected =
                    transform_stream_run_events_for_client("run-usage-boundary", completion);
                let usage = projected
                    .iter()
                    .find(|event| event["type"] == "usage")
                    .expect("C0 accounting event");
                let expected = sample
                    .map(CanonicalTokenUsage::to_json)
                    .unwrap_or(serde_json::Value::Null);
                assert_eq!(usage["qualified_usage"], expected);
                for key in [
                    "input_tokens",
                    "cached_input_tokens",
                    "cache_creation_tokens",
                    "output_tokens",
                    "total_tokens",
                ] {
                    assert_eq!(usage.get(key), expected.get(key));
                }
                let mut accum =
                    astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
                accum.qualified_usage = Some(known);
                accum.has_usage = true;
                accum.prompt_tokens = 7;
                accum.current_request_usage =
                    Some(astra_turn_types::RequestTokenUsage::try_new(7, 11, 2, 3).unwrap());
                for event in projected {
                    astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                        &format!("data: {}\n\n", serde_json::to_string(&event).unwrap()),
                        &mut accum,
                        &mut Vec::new(),
                    );
                }
                assert_eq!(accum.qualified_usage, sample);
                assert!(accum.usage_is_run_total);
                assert!(accum.current_request_usage.is_none());
                assert_eq!(
                    accum.prompt_tokens,
                    sample
                        .and_then(CanonicalTokenUsage::input_tokens)
                        .unwrap_or(0)
                );
                assert_eq!(
                    accum.completion_tokens,
                    sample
                        .and_then(CanonicalTokenUsage::output_tokens)
                        .unwrap_or(0)
                );
                assert_eq!(
                    accum.has_usage,
                    sample.is_some_and(|usage| !usage.to_json().as_object().unwrap().is_empty())
                );
            }
        }
    }

    #[tokio::test]
    async fn failed_c0_projection_survives_the_shared_stream_pump() {
        use astra_services::runs::{
            CanonicalRunTerminalReceipt, CanonicalTerminalContext, CanonicalTerminalOutcome,
            CanonicalTerminalTranscript,
        };
        use astra_turn_core::sse::stream_host::{NoopSseStreamHost, consume_sse_stream};
        let usage =
            astra_turn_types::CanonicalTokenUsage::new(Some(17), None, None, Some(3)).unwrap();
        for (error_code, error_kind) in [
            (
                Some("stream_transport"),
                Some(astra_core::ErrorKind::StreamTransport),
            ),
            (
                Some("insufficient_credit"),
                Some(astra_core::ErrorKind::Unknown),
            ),
            (None, Some(astra_core::ErrorKind::Network)),
            (
                Some("work_lifecycle_topology_conflict"),
                Some(astra_core::ErrorKind::ContractViolation),
            ),
            (Some("unclassified_business_failure"), None),
            (Some("stream_transport"), None),
        ] {
            let receipt = CanonicalRunTerminalReceipt {
                schema_version: 1,
                user_id: "owner".into(),
                session_id: "session".into(),
                run_id: "partial-run".into(),
                generation: 4,
                opening_hash: "a".repeat(64),
                context_admission_hash: None,
                outcome: CanonicalTerminalOutcome::Failed,
                error_hash: CanonicalRunTerminalReceipt::error_hash(Some("stream stopped")),
                error_code: error_code.map(str::to_owned),
                error_kind,
                error_metadata: None,
                interruption: None,
                context: CanonicalTerminalContext::Unowned,
                transcript: CanonicalTerminalTranscript {
                    item_count: 1,
                    digest: "b".repeat(64),
                    assistant_source_event_id: Some("source".into()),
                },
                usage: Some(usage),
                last_request_context: Some(astra_turn_types::TerminalRequestContext {
                    window: astra_turn_types::ContextWindowUsage::provider_reported(0, 8000),
                    request_identity: None,
                }),
                tool_ledger: None,
            };
            // Cold replay must preserve the same typed fact as live completion.
            let receipt: CanonicalRunTerminalReceipt =
                serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
            for terminal_only in [false, true] {
                let completion = receipt
                    .completion_events(
                        (!terminal_only).then_some("  observed partial bytes\n"),
                        Some("answer-id"),
                        Some("stream stopped"),
                        Some(1),
                        if terminal_only { 2 } else { -1 },
                        3,
                    )
                    .unwrap();
                if terminal_only {
                    assert_eq!(completion.len(), 1);
                    assert_eq!(completion[0]["event_type"], "run_finished");
                }
                let mut projected = vec![serde_json::json!({
                    "type":"session_info", "session_id":"session", "run_id":"partial-run"
                })];
                if terminal_only {
                    projected.push(
                        serde_json::json!({"type":"text_delta", "content":"  observed partial bytes\n"}),
                    );
                }
                projected.extend(transform_stream_run_events_for_client(
                    "partial-run",
                    completion,
                ));
                let frames: Vec<_> = projected
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect();
                for chunks in [
                    frames
                        .iter()
                        .map(|frame| Ok(frame.as_bytes().to_vec()))
                        .collect::<Vec<_>>(),
                    vec![Ok(frames.concat().trim_end().as_bytes().to_vec())],
                ] {
                    let mut source = futures_util::stream::iter(chunks);
                    let (result, abort) = consume_sse_stream(
                        &mut source,
                        &mut NoopSseStreamHost::default(),
                        std::time::Duration::from_secs(1),
                    )
                    .await;
                    // A terminal-only legacy failure has no classification and
                    // reports Unknown without granting authoritative closure.
                    // An unclassified run_error remains a protocol violation;
                    // it must not acquire authority from a later terminal.
                    let classified_kind = error_kind
                        .or_else(|| error_code.and_then(astra_core::ErrorKind::parse_tag));
                    let expected_kind = classified_kind.unwrap_or(if terminal_only {
                        astra_core::ErrorKind::Unknown
                    } else {
                        astra_core::ErrorKind::ContractViolation
                    });
                    let preserve = classified_kind
                        .is_some_and(|kind| kind != astra_core::ErrorKind::ContractViolation);
                    let expected_text = if preserve {
                        "  observed partial bytes\n"
                    } else {
                        ""
                    };
                    assert_eq!(
                        abort,
                        Some(expected_kind),
                        "code={error_code:?}, kind={error_kind:?}, terminal_only={terminal_only}, error={:?}",
                        result.accum.error_message
                    );
                    assert_eq!(result.accum.has_authoritative_failed_terminal(), preserve);
                    assert_eq!(result.accum.full_text, expected_text);
                    if preserve {
                        assert_eq!(
                            result.accum.model_item_id.as_deref(),
                            (!terminal_only).then_some("answer-id")
                        );
                        assert_eq!(result.accum.accounted_usage(), Some(usage));
                        assert_eq!(
                            result.accum.last_request_context,
                            receipt.last_request_context
                        );
                        assert_eq!(result.accum.measured_request_input_tokens(), Some(0));
                    }
                    assert!(!result.accum.stream_complete);
                    assert!(result.accum.server_execution_summary.is_none());
                    assert!(result.tool_results.is_empty());
                    let snapshot =
                astra_turn_core::agentic::turn_ingest::agentic_turn_stream_snapshot_from_sse_accum(
                    &result.accum,
                    None,
                );
                    assert_eq!(snapshot.full_text, expected_text);
                    if preserve {
                        assert_eq!(snapshot.qualified_usage, Some(usage));
                    }
                }
            }
        }
    }

    #[test]
    fn projected_terminal_usage_is_accepted_by_the_chat_sse_consumer() {
        let projected = transform_stream_run_events_for_client(
            "run-usage-boundary",
            vec![json!({
                "event_type": "run_finished",
                "data": {
                    "prompt_tokens": 7,
                    "cache_read_tokens": 11,
                    "cache_creation_tokens": 2,
                    "completion_tokens": 3
                },
                "index": 4
            })],
        );
        let usage = projected
            .iter()
            .find(|event| event["type"] == "usage")
            .expect("terminal projection must include usage");
        assert!(usage.get("prompt_tokens").is_none());
        assert!(usage.get("completion_tokens").is_none());

        let mut accumulated = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
            &format!("data: {}\n\n", serde_json::to_string(usage).unwrap()),
            &mut accumulated,
            &mut Vec::new(),
        );

        assert!(accumulated.error_message.is_none(), "{accumulated:?}");
        assert!(accumulated.has_usage);
        assert!(accumulated.usage_is_run_total);
        assert_eq!(accumulated.current_request_usage, None);
        assert_eq!(accumulated.prompt_tokens, 7);
        assert_eq!(accumulated.cache_read_tokens, 11);
        assert_eq!(accumulated.cache_creation_tokens, 2);
        assert_eq!(accumulated.completion_tokens, 3);
    }

    #[test]
    fn terminal_projection_keeps_accounting_total_separate_from_context_usage() {
        let projected = transform_stream_run_events_for_client(
            "run-usage-scope",
            vec![json!({
                "event_type": "run_finished",
                "data": {
                    "prompt_tokens": 2_127_556,
                    "cache_read_tokens": 1_706_112,
                    "cache_creation_tokens": 0,
                    "completion_tokens": 34_000,
                    "last_request_usage": {
                        "prompt_tokens": 17_250,
                        "cache_read_tokens": 85_248,
                        "cache_creation_tokens": 0,
                        "completion_tokens": 901
                    }
                }
            })],
        );
        assert_eq!(projected[0]["type"], "usage");
        assert_eq!(projected[0]["usage_scope"], "run_total");
        assert_eq!(projected[1]["type"], "context_usage");

        let mut accumulated = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        for event in projected.iter().take(2) {
            astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                &format!("data: {}\n\n", serde_json::to_string(event).unwrap()),
                &mut accumulated,
                &mut Vec::new(),
            );
        }
        assert_eq!(accumulated.prompt_tokens, 2_127_556);
        assert_eq!(
            accumulated.current_request_usage,
            Some(astra_turn_types::RequestTokenUsage {
                fresh_input_tokens: 17_250,
                cache_read_tokens: 85_248,
                cache_creation_tokens: 0,
                output_tokens: 901,
            })
        );
    }

    #[test]
    fn terminal_projection_preserves_partial_physical_input_without_context_pricing_lanes() {
        let projected = transform_stream_run_events_for_client(
            "run-partial-physical-input",
            vec![json!({
                "event_type": "run_finished",
                "data": {
                    "prompt_tokens": 10_000,
                    "cache_read_tokens": 90_000,
                    "cache_creation_tokens": 0,
                    "completion_tokens": 20,
                    "last_request_usage": null,
                    "last_request_input_tokens": 100_000
                }
            })],
        );
        assert_eq!(projected[0]["type"], "usage");
        assert_eq!(projected[0]["last_request_input_tokens"], 100_000);
        assert!(
            !projected
                .iter()
                .any(|event| event["type"] == "context_usage")
        );

        let mut accumulated = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        for event in &projected {
            astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                &format!("data: {}\n\n", serde_json::to_string(event).unwrap()),
                &mut accumulated,
                &mut Vec::new(),
            );
        }
        assert_eq!(accumulated.current_request_usage, None);
        assert_eq!(accumulated.measured_request_input_tokens(), Some(100_000));
    }

    #[test]
    fn malformed_terminal_usage_is_not_relabelled_as_zero() {
        for data in [
            json!({"prompt_tokens":"seven", "completion_tokens":3}),
            json!({"qualified_usage":{"input_tokens":"seven"}, "prompt_tokens":7,
                "cache_read_tokens":0,"cache_creation_tokens":0,"completion_tokens":3}),
        ] {
            let projected = transform_stream_run_events_for_client(
                "run-malformed-usage",
                vec![json!({"event_type":"run_finished", "data":data, "index":4})],
            );
            assert!(projected.iter().all(|event| event["type"] != "usage"));
            assert!(
                projected
                    .iter()
                    .any(|event| event["type"] == "run_finished")
            );
        }
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_interrupted_pause_status() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_interrupted",
                    "data": {
                        "kind": "budget_exhausted",
                        "resumable": true,
                        "user_message": "You can continue in the next message."
                    },
                    "index": 10
                }),
                json!({
                    "event_type": "run_finished",
                    "data": {
                        "interrupted": true,
                        "interruption_kind": "budget_exhausted",
                        "resumable": true,
                        "prompt_tokens": 7,
                        "completion_tokens": 3
                    },
                    "index": 11
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "run_interrupted",
                "run_id": "run-123",
                "kind": "budget_exhausted",
                "resumable": true,
                "user_message": "You can continue in the next message.",
                "message": "You can continue in the next message.",
                "index": 10
            })
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "usage",
                "input_tokens": 7,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 3,
                "total_tokens": 10,
                "usage_scope": "run_total"
            })
        );
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_finished",
                "run_id": "run-123",
                "status": "paused",
                "interrupted": true,
                "interruption_kind": "budget_exhausted",
                "resumable": true,
                "index": 11
            })
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_run_started() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "run_started",
                "data": {},
                "index": 1
            })],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_started", "run_id": "run-123", "index": 1})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_pause_resume_events() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_paused",
                    "data": {},
                    "index": 2
                }),
                json!({
                    "event_type": "run_waiting",
                    "data": {"reason": "waiting: executor_offline"},
                    "index": 3
                }),
                json!({
                    "event_type": "run_resumed",
                    "data": {},
                    "index": 4
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({"type": "run_paused", "run_id": "run-123", "index": 2})
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "run_waiting",
                "run_id": "run-123",
                "reason": "waiting: executor_offline",
                "index": 3
            })
        );
        assert_eq!(
            transformed[2],
            json!({"type": "run_resumed", "run_id": "run-123", "index": 4})
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_emits_typed_intent_acceptance() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "event_type": "user_intent",
                "data": {
                    "intent_id": "intent-5",
                    "delivery": "guide_current_run",
                    "input": {"content": "focus the test"}
                },
                "index": 5
            })],
        );

        assert_eq!(
            transformed,
            vec![json!({
                "type": "user_intent_accepted",
                "run_id": "run-123",
                "intent_id": "intent-5",
                "delivery": "guide_current_run",
                "status": "accepted_remote",
                "index": 5
            })]
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_preserves_applied_intent_identity() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "type": "user_intent_applied",
                "intent_id": "input-7",
                "delivery": "guide_current_run",
                "status": "applied",
                "event_index": 7,
                "content": "change course",
                "index": 8,
            })],
        );

        assert_eq!(
            transformed,
            vec![json!({
                "type": "user_intent_applied",
                "run_id": "run-123",
                "intent_id": "input-7",
                "delivery": "guide_current_run",
                "status": "applied",
                "event_index": 7,
                "content": "change course",
                "index": 8,
            })]
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_preserves_returned_intent_identity() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![json!({
                "type": "user_intent_returned",
                "intent_id": "input-8",
                "delivery": "guide_current_run",
                "status": "returned",
                "event_index": 8,
                "content": "preserve this draft",
                "index": 9,
            })],
        );
        assert_eq!(
            transformed,
            vec![json!({
                "type": "user_intent_returned",
                "run_id": "run-123",
                "intent_id": "input-8",
                "delivery": "guide_current_run",
                "status": "returned",
                "event_index": 8,
                "content": "preserve this draft",
                "index": 9,
            })]
        );
    }

    #[test]
    fn transform_stream_run_events_for_client_injects_run_id_into_blocked_events() {
        let transformed = transform_stream_run_events_for_client(
            "run-123",
            vec![
                json!({
                    "event_type": "run_blocked", "reason": "executor_offline",
                    "data": {
                        "message": "Execution provider is offline.",
                        "reason": "executor_offline"
                    },
                    "index": 4
                }),
                json!({
                    "type": "run_blocked", "reason": "transport_disconnected",
                    "reason": "transport_disconnected",
                    "index": 5
                }),
                json!({
                    "event_type": "run_blocked", "reason": "route_mismatch",
                    "data": {
                        "reason": "route_mismatch",
                        "workspace": {"kind": "cloud_workspace"},
                        "executor": {"kind": "orchestrator_managed", "status": "degraded"}
                    },
                    "index": 6
                }),
            ],
        );

        assert_eq!(
            transformed[0],
            json!({
                "type": "run_blocked", "reason": "executor_offline",
                "run_id": "run-123",
                "message": "Execution provider is offline.",
                "reason": "executor_offline",
                "index": 4
            })
        );
        assert_eq!(
            transformed[1],
            json!({
                "type": "run_blocked", "reason": "transport_disconnected",
                "run_id": "run-123",
                "reason": "transport_disconnected",
                "index": 5
            })
        );
        assert_eq!(
            transformed[2],
            json!({
                "type": "run_blocked", "reason": "route_mismatch",
                "run_id": "run-123",
                "reason": "route_mismatch",
                "workspace": {"kind": "cloud_workspace"},
                "executor": {"kind": "orchestrator_managed", "status": "degraded"},
                "index": 6
            })
        );
    }

    #[tokio::test]
    async fn stream_run_http_replays_through_multiple_pages_after_cache_eviction() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-durable-http", "u1", "session-http")
            .await
            .expect("start durable run");
        for index in 0..260 {
            engine
                .append_event(
                    "u1",
                    "session-http",
                    "run-durable-http",
                    json!({"event_type": "text_done", "data": {"full_text": format!("answer page item {index:03}")}}),
                )
                .await
                .expect("persist replay page item");
        }
        engine
            .append_event(
                "u1",
                "session-http",
                "run-durable-http",
                json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
            )
            .await
            .expect("persist text_done");
        engine
            .append_event(
                "u1",
                "session-http",
                "run-durable-http",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 2, "completion_tokens": 1}}),
            )
            .await
            .expect("persist run_finished");
        assert!(
            engine
                .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                    user_id: "u1",
                    expected_session_id: "session-http",
                    run_id: "run-durable-http",
                    status: astra_core::STATUS_COMPLETED,
                    waiting_for: None,
                    error_message: None,
                    expected_statuses: &["running"],
                })
                .await
                .expect("mark completed")
        );
        engine
            .append_event(
                "u1",
                "session-http",
                "run-durable-http",
                json!({"event_type": "text_done", "data": {"full_text": "recorded after terminal"}}),
            )
            .await
            .expect("persist later durable event");

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine.clone(),
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-durable-http/stream?last_index=1")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        let mut offset = 0;
        for index in 0..260 {
            let item = format!("answer page item {index:03}");
            let position = text[offset..].find(&item).expect("ordered replay item");
            assert_eq!(text.matches(&item).count(), 1);
            offset += position + item.len();
        }
        assert!(text[offset..].contains("durable final answer"));
        assert!(
            text.contains("\"type\":\"text_done\""),
            "durable replay should keep final answer event: {text}"
        );
        assert!(
            text.contains("\"full_text\":\"durable final answer\""),
            "durable replay should expose final answer text to the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"usage\""),
            "run_finished usage should still be transformed for the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\""),
            "terminal lifecycle event should still reach the client: {text}"
        );
        assert!(text.contains("recorded after terminal"));
        assert!(!text.contains("\"error_kind\":\"contract_violation\""));
        assert!(!text.contains("stream_observation_interrupted"));

        let replay = app
            .oneshot(
                Request::builder()
                    .uri("/chat/runs/run-durable-http/stream?last_index=1&replay_only=true")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        engine.append_event(
            "u1", "session-http", "run-durable-http",
            json!({"event_type": "text_done", "data": {"full_text": "appended above replay ceiling"}}),
        ).await.unwrap();
        let replay_body = body::to_bytes(replay.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let replay_text = std::str::from_utf8(&replay_body).unwrap();
        assert!(replay_text.contains("answer page item 000"));
        assert!(replay_text.contains("answer page item 259"));
        assert!(replay_text.contains("recorded after terminal"));
        assert!(!replay_text.contains("appended above replay ceiling"));
        assert!(replay_text.ends_with("data: [DONE]\n\n"));
    }

    #[tokio::test]
    async fn stream_run_accepts_provider_request_principal() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run(
                "run-provider-stream",
                "provider-user-1",
                "session-provider-stream",
            )
            .await
            .expect("start provider-owned run");
        engine
            .append_event(
                "provider-user-1",
                "session-provider-stream",
                "run-provider-stream",
                json!({"event_type": "run_finished", "data": {"status": "completed"}}),
            )
            .await
            .expect("persist run_finished");
        assert!(
            engine
                .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                    user_id: "provider-user-1",
                    expected_session_id: "session-provider-stream",
                    run_id: "run-provider-stream",
                    status: astra_core::STATUS_COMPLETED,
                    waiting_for: None,
                    error_message: None,
                    expected_statuses: &["running"],
                })
                .await
                .expect("mark completed")
        );

        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );
        let descriptor = Arc::new(Mutex::new(None));
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(ProviderRequestOnlyAuthService {
                    descriptor: Arc::clone(&descriptor),
                    scope: None,
                }))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-provider-stream/stream?last_index=0")
                    .header("authorization", "Bearer provider-token")
                    .header("x-request-id", "provider-stream-request")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(text.contains("\"type\":\"run_finished\""), "SSE = {text}");

        let descriptor = descriptor
            .lock()
            .expect("descriptor lock")
            .clone()
            .expect("provider request descriptor");
        assert_eq!(descriptor.method, "GET");
        assert_eq!(descriptor.path, "/chat/runs/run-provider-stream/stream");
        assert_eq!(
            descriptor.route.as_deref(),
            Some("/chat/runs/{run_id}/stream")
        );
        assert_eq!(
            descriptor.request_id.as_deref(),
            Some("provider-stream-request")
        );
        assert_eq!(
            descriptor.body_digest.as_deref(),
            Some("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires MatrixOne DB: exercise cross-server HTTP and WebSocket observation"]
    async fn remote_run_observers_batch_idle_reads_and_reconnect_over_both_transports() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::DatabaseRunStateStore;
        use futures_util::{SinkExt, StreamExt, future::try_join_all};
        use serde_json::Value;
        use std::time::Duration;
        use tokio_tungstenite::{connect_async, tungstenite::Message};

        let owner_pool = setup_http_run_db_it().await;
        let observer_pool = astra_core::SharedPool::new(owner_pool.settings())
            .await
            .unwrap();
        let run_id = format!("remote-{}", uuid::Uuid::new_v4());
        let owner = RunEngine::new(Arc::new(
            DatabaseRunStateStore::new(owner_pool.clone()).with_owner_pod_id("observer-test-owner"),
        ));
        let observer = RunEngine::new(Arc::new(
            DatabaseRunStateStore::new(observer_pool.clone())
                .with_owner_pod_id("observer-test-peer"),
        ));
        let run_ids = [run_id.clone(), format!("remote-{}", uuid::Uuid::new_v4())];
        let mut generations = Vec::new();
        for id in &run_ids {
            crate::server::run::insert_active_run_session_fixture(&owner_pool, "u1", id).await;
            generations.push(
                owner
                    .start_run(id, "u1", id)
                    .await
                    .unwrap()
                    .owner_generation,
            );
        }
        let mut servers = Vec::new();
        for engine in [owner.clone(), observer] {
            let app = build_app(
                AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                    .with_auth_service(Arc::new(StubAuthService))
                    .with_run_lifecycle_service(Arc::new(AgenticRunLifecycleService::new(
                        owner_pool.settings().clone(),
                        Arc::new(crate::FernetTokenEncryptor::new("0123456789abcdef").unwrap()),
                        Arc::new(TokioMutex::new(HashMap::new())),
                        engine,
                    ))),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            servers.push((
                addr,
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
            ));
        }
        let warm = try_join_all(
            (0..observer_pool.settings().db_pool_max_connections)
                .map(|_| observer_pool.get().acquire()),
        )
        .await
        .unwrap();
        drop(warm);
        let addr = servers[1].0;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut subscribers = Vec::new();
        for index in 0..32 {
            let id = &run_ids[index % run_ids.len()];
            let response = client
                .get(format!("http://{addr}/chat/runs/{id}/stream"))
                .bearer_auth("good-token")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            subscribers.push(response);
            tokio::time::sleep(Duration::from_millis(45)).await;
        }

        let connect = |addr, index| {
            let run_id = run_id.clone();
            async move {
                let (mut socket, _) = connect_async(format!("ws://{addr}/chat/ws")).await.unwrap();
                for frame in [
                    json!({"type":"auth","token":"Bearer good-token","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                    json!({"type":"attach_run","run_id":run_id,"last_index":index}),
                ] {
                    socket
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .unwrap();
                    loop {
                        if let Message::Text(body) = socket.next().await.unwrap().unwrap() {
                            let event: Value = serde_json::from_str(&body).unwrap();
                            let expected = if frame["type"] == "auth" {
                                "auth_ok"
                            } else {
                                "session_info"
                            };
                            assert_eq!(event["type"], expected, "{event}");
                            break;
                        }
                    }
                }
                socket
            }
        };
        let mut socket = tokio::time::timeout(Duration::from_secs(5), connect(addr, 0))
            .await
            .unwrap();

        // Let every stream reach steady state, then count physical pooled SQL
        // acquisitions over more than one full idle interval.
        tokio::time::sleep(Duration::from_millis(4100)).await;
        let before = astra_core::matrixone_pool_telemetry_snapshot().hot_checkouts;
        tokio::time::sleep(Duration::from_millis(2250)).await;
        let idle = astra_core::matrixone_pool_telemetry_snapshot().hot_checkouts - before;
        println!("remote observers=33 runs=2 stagger_ms=45 idle_ms=2250 pooled_sql={idle}");
        assert!((1..=8).contains(&idle), "idle reads did not batch: {idle}");

        // A remote browser must see tool progress while the owner is still
        // running. A terminal-only replay cannot satisfy live observation.
        owner
            .append_event(
                "u1",
                &run_id,
                &run_id,
                json!({"type":"tool_call_end","call_id":"live-tool","tool":"bash","result":"ready"}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Message::Text(body) = socket.next().await.unwrap().unwrap() {
                    let event: Value = serde_json::from_str(&body).unwrap();
                    assert_ne!(event["type"], "run_finished");
                    assert_ne!(event["type"], "error", "{event}");
                    if event["type"] == "tool_call_end" {
                        assert_eq!(event["call_id"], "live-tool");
                        break;
                    }
                }
            }
        })
        .await
        .expect("live tool result from owner Server");

        for (id, generation) in run_ids.iter().zip(generations) {
            assert!(owner.transition_status_with_events_if_current_owner(
                "u1", id, id, &[astra_core::STATUS_RUNNING], generation,
                astra_core::STATUS_COMPLETED, None, None, &[
                    json!({"event_type":"text_done","data":{"full_text":format!("answer from {id}")}}),
                    json!({"event_type":"run_finished","data":{"status":"completed"}}),
                ],
            ).await.unwrap());
        }

        let terminal_index = tokio::time::timeout(Duration::from_secs(10), async {
            let mut saw_answer = false;
            loop {
                if let Message::Text(body) = socket.next().await.unwrap().unwrap() {
                    let event: Value = serde_json::from_str(&body).unwrap();
                    saw_answer |= event["full_text"] == format!("answer from {run_id}");
                    assert_ne!(event["type"], "error", "{event}");
                    if event["type"] == "run_finished" {
                        assert!(saw_answer);
                        return event["index"].as_u64().unwrap();
                    }
                }
            }
        })
        .await
        .unwrap();
        let bodies = tokio::time::timeout(
            Duration::from_secs(10),
            try_join_all(subscribers.into_iter().map(reqwest::Response::text)),
        )
        .await
        .unwrap()
        .unwrap();
        for (index, body) in bodies.into_iter().enumerate() {
            let id = &run_ids[index % run_ids.len()];
            assert!(body.contains(&format!("answer from {id}")), "{body}");
            assert!(
                body.contains("run_finished") && body.contains("data: [DONE]"),
                "{body}"
            );
            assert!(!body.contains("stream_observation_interrupted"), "{body}");
        }
        let mut reattached = tokio::time::timeout(
            Duration::from_secs(5),
            connect(servers[0].0, terminal_index + 1),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Message::Text(body) = reattached.next().await.unwrap().unwrap() {
                    let event: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(event["type"], "run_finished");
                    assert_eq!(event["status"], "completed");
                    break;
                }
            }
        })
        .await
        .unwrap();
        for (_, server) in servers {
            server.abort();
        }
        for id in &run_ids {
            cleanup_run_http_fixture(&owner_pool, "u1", id).await;
            sqlx::query("DELETE FROM agent_sessions WHERE user_id=? AND session_id=?")
                .bind("u1")
                .bind(id)
                .execute(owner_pool.get())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn stream_run_http_replays_matrixone_cache_miss_durable_events() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
        use sqlx::Row;
        use uuid::Uuid;

        let shared_pool = setup_http_run_db_it().await;
        let settings = shared_pool.settings().clone();
        let user_id = "u1";
        let run_id = format!("run-replay-http-it-{}", Uuid::new_v4());
        let session_id = format!("session-replay-http-it-{}", Uuid::new_v4());
        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;
        crate::server::run::insert_active_run_session_fixture(&shared_pool, user_id, &session_id)
            .await;

        let store: Arc<dyn RunStateStore> = Arc::new(
            DatabaseRunStateStore::new(shared_pool.clone())
                .with_owner_pod_id("stream-replay-http-it-pod"),
        );
        let engine = RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, user_id, &session_id)
            .await
            .expect("start durable DB run");
        let durable_events = vec![
            json!({
                "type": "tool_call",
                "tool_call": {"id": "call-1", "name": "bash", "arguments": {"cmd": "printf ok"}}
            }),
            json!({
                "type": "tool_call_end",
                "call_id": "call-1",
                "tool": "bash",
                "result": "ok"
            }),
            json!({
                "event_type": "approval_required",
                "data": {
                    "approval_id": "approval-1",
                    "request_id": "approval-1",
                    "tool": "bash",
                    "approval_kind": "standard"
                }
            }),
            json!({
                "event_type": "user_input",
                "data": {
                    "request_id": "approval-1",
                    "text": "approved from another pod"
                }
            }),
            json!({
                "event_type": "text_done",
                "data": {"full_text": "durable final answer from matrixone"}
            }),
            json!({
                "event_type": "run_finished",
                "data": {"prompt_tokens": 2, "completion_tokens": 1, "tool_call_count": 1}
            }),
        ];
        let transitioned = engine
            .transition_status_with_events_if_current_owner(
                user_id,
                &session_id,
                &run_id,
                &[astra_core::STATUS_RUNNING],
                authority.owner_generation,
                astra_core::STATUS_COMPLETED,
                None,
                None,
                &durable_events,
            )
            .await
            .expect("commit terminal durable DB events");
        assert!(transitioned);

        let first_page = engine
            .load_run_event_window(user_id, &run_id, -1, 2)
            .await
            .expect("read first durable page")
            .expect("run exists");
        assert!(first_page.last_event_idx > 2);
        assert!(
            first_page
                .events
                .iter()
                .all(|event| event["index"].as_i64().is_some_and(|index| index <= 2))
        );
        let next_page = engine
            .load_run_event_window(user_id, &run_id, 2, first_page.last_event_idx)
            .await
            .expect("read remaining durable events")
            .expect("run exists");
        let full = engine
            .load_run_event_delta(user_id, &run_id, -1)
            .await
            .expect("read full replay")
            .expect("run exists");
        assert_eq!([first_page.events, next_page.events].concat(), full.events);

        let persisted_event_types = sqlx::query(
            "SELECT event_type FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx ASC",
        )
        .bind(user_id)
        .bind(&run_id)
        .fetch_all(shared_pool.get())
        .await
        .expect("load persisted event types")
        .into_iter()
        .map(|row| row.try_get::<String, _>("event_type").expect("event_type"))
        .collect::<Vec<_>>();
        assert_eq!(
            persisted_event_types,
            vec![
                "tool_call",
                "tool_call_end",
                "approval_required",
                "user_input",
                "text_done",
                "run_finished",
            ],
            "opening is virtual; later semantic facts remain physical"
        );

        let lifecycle = AgenticRunLifecycleService::new(
            settings,
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/chat/runs/{run_id}/stream?last_index=1"))
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let text = String::from_utf8(body.to_vec()).expect("sse should be utf8");
        assert!(
            text.contains("\"type\":\"tool_call\""),
            "durable replay should expose tool start boundary: {text}"
        );
        assert!(
            text.contains("\"type\":\"tool_call_end\""),
            "durable replay should expose tool end boundary: {text}"
        );
        assert!(
            text.contains("\"type\":\"approval_required\"")
                && text.contains("\"request_id\":\"approval-1\""),
            "durable replay should expose approval request boundary: {text}"
        );
        assert!(
            text.contains("\"type\":\"user_input\"")
                && text.contains("\"text\":\"approved from another pod\""),
            "durable replay should expose approval/user input response boundary: {text}"
        );
        assert!(
            text.contains("\"full_text\":\"durable final answer from matrixone\""),
            "durable replay should expose final answer text: {text}"
        );
        assert!(
            text.contains("\"type\":\"usage\""),
            "run_finished usage should still be transformed for the client: {text}"
        );
        assert!(
            text.contains("\"type\":\"run_finished\"") && text.contains("\"status\":\"completed\""),
            "terminal lifecycle event should still reach the client: {text}"
        );
        assert!(
            !text.contains("text_delta") && !text.contains("agent_live_event"),
            "cache-miss replay should not require transport-only live deltas: {text}"
        );

        cleanup_run_http_fixture(&shared_pool, user_id, &run_id).await;
        crate::server::run::cleanup_run_session_fixture(&shared_pool, user_id, &session_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn replay_window_preserves_large_records_and_bounds_the_page() {
        use crate::server::run::engine::RunEngine;
        use astra_services::runs::{DatabaseRunStateStore, RUN_REPLAY_PAGE_BYTES};
        use astra_turn_types::json_serialized_len;

        let pool = setup_http_run_db_it().await;
        let run_id = uuid::Uuid::new_v4().to_string();
        let user = "u1";
        crate::server::run::insert_active_run_session_fixture(&pool, user, &run_id).await;
        let engine = RunEngine::new(Arc::new(DatabaseRunStateStore::new(pool.clone())));
        engine.start_run(&run_id, user, &run_id).await.unwrap();

        let text = "é".repeat(256 * 1024);
        let events = (0..6)
            .map(|n| json!({"event_type": "text_done", "data": {"full_text": text, "part": n}}))
            .collect::<Vec<_>>();
        engine
            .append_events_batch(user, &run_id, &run_id, &events)
            .await
            .unwrap();

        let mut cursor = -1;
        let mut parts = Vec::new();
        let mut page_count = 0;
        loop {
            let page = engine
                .load_run_event_window(user, &run_id, cursor, cursor + 128)
                .await
                .unwrap()
                .unwrap();
            page_count += 1;
            let event_bytes = page
                .events
                .iter()
                .map(|event| json_serialized_len(event).unwrap())
                .sum::<u64>();
            assert!(event_bytes <= RUN_REPLAY_PAGE_BYTES);
            assert!(page.through_event_idx > cursor);
            for event in page.events {
                if let Some(part) = event["data"]["part"].as_u64() {
                    assert_eq!(event["data"]["full_text"], text.as_str());
                    parts.push(part);
                }
            }
            cursor = page.through_event_idx;
            if cursor == page.last_event_idx {
                break;
            }
        }
        assert_eq!(parts, (0..6).collect::<Vec<_>>());
        assert_eq!(page_count, 2);
        cleanup_run_http_fixture(&pool, user, &run_id).await;
        crate::server::run::cleanup_run_session_fixture(&pool, user, &run_id).await;
    }

    #[tokio::test]
    async fn get_run_projection_http_returns_bounded_projection_and_filters_internal_events() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        let authority = engine
            .start_run("run-projection-http", "u1", "session-projection")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "u1",
                "session-projection",
                "run-projection-http",
                json!({
                    "event_type": "workspace_bound",
                    "data": {
                        "workspace": {
                            "kind": "edge_workspace",
                            "display_name": "MacBook Pro",
                            "cwd": "/workspace/astra",
                            "authority": "read_write"
                        },
                        "executor": {
                            "kind": "edge_agent",
                            "executor_id": "edge-macbook-1",
                            "display_name": "MacBook Pro",
                            "transport": "edge_ws",
                            "status": "online"
                        },
                        "transport": "edge_ws"
                    }
                }),
            )
            .await
            .expect("persist binding event");
        engine
            .append_event(
                "u1",
                "session-projection",
                "run-projection-http",
                json!({"event_type": "injection_freshness", "data": {"fingerprint": "secret"}}),
            )
            .await
            .expect("persist internal event");
        engine
            .append_event(
                "u1",
                "session-projection",
                "run-projection-http",
                json!({"event_type": "text_done", "data": {"full_text": "durable answer"}}),
            )
            .await
            .expect("persist final answer");
        engine
            .append_event(
                "u1",
                "session-projection",
                "run-projection-http",
                json!({"event_type": "run_error", "data": {"error": "boom"}}),
            )
            .await
            .expect("persist error");
        engine
            .append_event(
                "u1",
                "session-projection",
                "run-projection-http",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 5, "completion_tokens": 2}}),
            )
            .await
            .expect("persist run finished");
        engine
            .persist_usage_if_current_owner(
                "u1",
                "session-projection",
                "run-projection-http",
                authority.owner_generation,
                5,
                2,
                0,
            )
            .await
            .expect("persist usage");
        engine
            .persist_checkpoint(
                "u1",
                "session-projection",
                "run-projection-http",
                r#"{"version":"checkpoint_v3","graceful":true,"last_batch_id":"batch-run-projection"}"#,
            )
            .await
            .expect("persist checkpoint");
        assert!(
            engine
                .persist_status_if_current(astra_services::runs::RunStatusCasRequest {
                    user_id: "u1",
                    expected_session_id: "session-projection",
                    run_id: "run-projection-http",
                    status: astra_core::STATUS_FAILED,
                    waiting_for: None,
                    error_message: Some("boom"),
                    expected_statuses: &["running"],
                })
                .await
                .expect("mark failed")
        );

        engine
            .start_run("active-projection-http", "u1", "session-projection")
            .await
            .unwrap();
        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-projection-http/projection?recent_limit=4")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .expect("body should be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("projection response should be valid json");
        assert_eq!(json.get("status"), Some(&json!("failed")));
        assert!(
            json.get("total_tool_calls")
                .is_some_and(serde_json::Value::is_number)
        );
        assert_eq!(json.get("latest_event_type"), Some(&json!("run_finished")));
        assert_eq!(json.get("run_event_high_watermark"), Some(&json!(5)));
        assert_eq!(
            json.pointer("/workspace/kind"),
            Some(&json!("edge_workspace"))
        );
        assert_eq!(
            json.pointer("/workspace/cwd"),
            Some(&json!("/workspace/astra"))
        );
        assert_eq!(json.pointer("/executor/kind"), Some(&json!("edge_agent")));
        assert_eq!(
            json.pointer("/executor/executor_id"),
            Some(&json!("edge-macbook-1"))
        );
        assert_eq!(json.get("transport"), Some(&json!("edge_ws")));
        assert_eq!(
            json.pointer("/latest_checkpoint/checkpoint_version"),
            Some(&json!("checkpoint_v3"))
        );
        assert!(json.get("observability").is_none());
        let recent_events = json
            .get("recent_events")
            .and_then(serde_json::Value::as_array)
            .expect("recent_events should be an array");
        assert_eq!(recent_events.len(), 4);
        assert!(
            recent_events.iter().all(|event| {
                event.get("type").and_then(serde_json::Value::as_str) != Some("workspace_bound")
            }),
            "projection top-level binding must not depend on the bounded recent event window"
        );
        assert!(
            recent_events.iter().all(|event| {
                event.get("type").and_then(serde_json::Value::as_str) != Some("injection_freshness")
            }),
            "internal events must stay out of the public projection payload"
        );
        assert!(
            recent_events
                .iter()
                .any(|event| event.get("type") == Some(&json!("usage"))),
            "run projection should expose transformed usage data"
        );
        assert!(
            recent_events
                .iter()
                .any(|event| event.get("full_text") == Some(&json!("durable answer"))),
            "run projection should keep bounded durable transcript data"
        );
        let active = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/active-projection-http/projection")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(active.status(), StatusCode::OK);
        let bytes = body::to_bytes(active.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let active: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(active["status"], "running");
        assert!(
            active
                .get("total_tool_calls")
                .is_some_and(serde_json::Value::is_number)
        );
    }

    #[tokio::test]
    async fn retired_display_repair_route_returns_not_found() {
        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService)),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/chat/runs/retired-display/projection/repair")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_run_projection_http_hides_foreign_run() {
        use crate::server::run::engine::RunEngine;
        use crate::server::run::lifecycle::AgenticRunLifecycleService;
        use astra_services::runs::InMemoryRunStateStore;

        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        engine
            .start_run("run-foreign", "u2", "session-foreign")
            .await
            .expect("start durable run");
        let lifecycle = AgenticRunLifecycleService::new(
            test_matrixone(),
            Arc::new(
                crate::FernetTokenEncryptor::new("0123456789abcdef")
                    .expect("test encryptor should initialize"),
            ),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );

        let app = build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_auth_service(Arc::new(StubAuthService))
                .with_run_lifecycle_service(Arc::new(lifecycle)),
        );

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/chat/runs/run-foreign/projection")
                    .header("authorization", "Bearer good-token")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("response should be returned");

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
