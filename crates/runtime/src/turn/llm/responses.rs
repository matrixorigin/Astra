//! Responses execution under the ordinary inference-attempt owner.
use super::*;
use astra_core::model_wire::responses::{
    ResponseAccumulator, ResponseOutput, ResponsesStreamError,
};

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;

pub(super) fn error(kind: astra_core::ErrorKind, code: &str) -> astra_core::ClassifiedError {
    let message = match code {
        "unsupported_responses_generation_settings" => {
            "The selected model cannot use the requested generation settings".to_string()
        }
        "unsupported_responses_reasoning_control" => {
            "The selected model does not support the requested reasoning setting".to_string()
        }
        "unsupported_responses_input" => "The selected model cannot process this input".to_string(),
        "responses_continuation_scope_changed" => {
            "The model configuration changed during this turn. Start a new turn to continue."
                .to_string()
        }
        _ => format!("Model request failed: {code}"),
    };
    astra_core::ClassifiedError::new(kind, message)
        .with_details_json(json!({"source":"responses","code":code}).to_string())
}
pub(super) fn credential_error(
    value: (
        axum::http::StatusCode,
        axum::Json<astra_core::ErrorResponse>,
    ),
) -> astra_core::ClassifiedError {
    let code = value
        .1
        .0
        .error_code
        .as_deref()
        .unwrap_or("model_connection_unavailable");
    let kind = if value.0 == axum::http::StatusCode::CONFLICT {
        astra_core::ErrorKind::Auth
    } else {
        provider_http_error_kind(value.0.as_u16(), code)
    };
    error(kind, code)
}

pub(super) fn prepare(
    call: LlmCall<'_>,
    tool_choice: RuntimeToolChoice,
    started: Instant,
) -> Result<PreparedStreamingCall<'_>, astra_core::ClassifiedError> {
    if call.cache_capability.is_some_and(|capability| {
        matches!(
            capability.protocol,
            astra_turn_core::cache_placement::CacheProtocol::MarkerExplicit
                | astra_turn_core::cache_placement::CacheProtocol::BedrockCachePoint
        )
    }) {
        return Err(error(
            astra_core::ErrorKind::ContractViolation,
            "unsupported_responses_cache_placement",
        ));
    }
    if let Some(credential) = call.route.subscription {
        let fixed_base = credential.api_base().map_err(credential_error)?;
        if call.route.base_url != fixed_base || call.route.completions_url_override.is_some() {
            return Err(error(
                astra_core::ErrorKind::ContractViolation,
                "subscription_endpoint_mismatch",
            ));
        }
    } else if call.route.completions_url_override.is_some() {
        return Err(error(
            astra_core::ErrorKind::ContractViolation,
            "responses_endpoint_override_unsupported",
        ));
    }
    let scope = call
        .route
        .subscription
        .map(|credential| credential.connection_id())
        .or(call.route.response_scope)
        .ok_or_else(|| {
            error(
                astra_core::ErrorKind::ContractViolation,
                "response_scope_missing",
            )
        })?;
    if call
        .route
        .request_body_overrides
        .is_some_and(|values| !values.is_empty())
        || call.route.fixed_temperature.is_some()
        || (call.route.subscription.is_none() && call.temperature.is_some())
    {
        return Err(error(
            astra_core::ErrorKind::InvalidRequest,
            if call.route.subscription.is_some() {
                "unsupported_subscription_generation_settings"
            } else {
                "unsupported_responses_generation_settings"
            },
        ));
    }
    let api_off = call.route.subscription.is_none()
        && matches!(call.thinking, ThinkingConfig::Off)
        && call
            .route
            .supported_reasoning_efforts
            .is_some_and(|efforts| efforts.iter().any(|effort| effort == "none"));
    if !(matches!(
        call.thinking,
        ThinkingConfig::ModelDefault | ThinkingConfig::Adaptive { .. }
    ) || api_off)
        || matches!(call.thinking, ThinkingConfig::Adaptive { .. })
            && (call.route.thinking_protocol
                != Some(astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning)
                || call.route.subscription.is_some()
                    && call.route.supported_reasoning_efforts.is_none())
    {
        return Err(error(
            astra_core::ErrorKind::InvalidRequest,
            if call.route.subscription.is_some() {
                "unsupported_subscription_reasoning_control"
            } else {
                "unsupported_responses_reasoning_control"
            },
        ));
    }
    call.validate_declared_reasoning()?;
    let model = call.route.wire_model_name.unwrap_or(call.route.model_name);
    // Decide whether this is a new human turn before runtime-owned context is
    // projected into user-role messages for the provider.
    let active_start = call
        .messages
        .iter()
        .rposition(astra_turn_types::is_human_user_message)
        .map_or(0, |index| index + 1);
    if call.messages[active_start..].iter().any(|message| {
        message["role"] == "assistant"
            && message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            && message
                .get(astra_core::model_wire::responses::CONTINUATION_KEY)
                .is_some_and(|saved| saved["connection_id"] != scope || saved["model"] != model)
    }) {
        return Err(error(
            astra_core::ErrorKind::InvalidRequest,
            "responses_continuation_scope_changed",
        ));
    }
    // Direct and pre-projected calls share runtime authority/order, continuation
    // and provider-call-ID projection before the codec encodes the request.
    let messages = consolidate_system_messages_for_protocol(
        call.messages,
        call.route.provider,
        call.cache_capability,
        LlmProviderProtocol::OpenAiResponses,
    );
    let mut tools = call.tools.to_vec();
    for tool in &mut tools {
        strip_internal_schema_extensions(tool);
    }
    let mut body = astra_core::model_wire::responses::request(
        model,
        &messages,
        Some(&tools),
        scope,
        matches!(tool_choice, RuntimeToolChoice::None),
        call.max_output_tokens,
    )
    .map_err(|cause| {
        error(
            astra_core::ErrorKind::InvalidRequest,
            if cause == astra_core::model_wire::responses::CONTINUATION_SCOPE_CHANGED {
                "responses_continuation_scope_changed"
            } else if call.route.subscription.is_some() {
                "unsupported_subscription_input"
            } else {
                "unsupported_responses_input"
            },
        )
    })?;
    if call.route.subscription.is_none() {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    call.thinking.apply_openai_protocol(
        &mut body,
        astra_core::model_wire::thinking::ThinkingProtocol::ResponsesReasoning,
    );
    if api_off {
        body["reasoning"] = json!({"effort":"none"});
    }
    let authorized_tool_names = if matches!(tool_choice, RuntimeToolChoice::None) {
        HashSet::new()
    } else {
        call.tools
            .iter()
            .filter_map(tool_schema_name)
            .filter_map(canonical_valid_tool_name)
            .map(str::to_owned)
            .collect()
    };
    let judgment_provenance = discrete_judgment_provenance(&call);
    let request = PreparedProviderRequest::from_json_with_cache_capability(
        &body,
        LlmProviderProtocol::OpenAiResponses,
        call.cache_capability,
    )?;
    let url = format!("{}/responses", call.route.base_url.trim_end_matches('/'));
    Ok(PreparedStreamingCall {
        purpose: call.purpose,
        route: call.route,
        request,
        wire_output_limit: provider_request_output_limit(&body),
        authorized_tool_names,
        url,
        judgment_provenance,
        preparation_elapsed: started.elapsed(),
    })
}

fn normalized_usage(
    raw: &serde_json::Map<String, serde_json::Value>,
) -> (
    serde_json::Map<String, serde_json::Value>,
    crate::turn::token_usage::TokenUsagePresence,
) {
    crate::turn::token_usage::parse_usage(crate::turn::token_usage::UsageDialect::OpenAi, raw)
        .map(|(usage, presence)| (usage.to_qualified_json_map(presence), presence))
        .unwrap_or_default()
}

fn invalid_aliases(values: [Option<&serde_json::Value>; 2]) -> bool {
    let mut previous = None;
    for value in values
        .into_iter()
        .flatten()
        .filter(|value| !value.is_null())
    {
        let count = value.as_u64().filter(|count| i64::try_from(*count).is_ok());
        if count.is_none() || previous.is_some_and(|previous| Some(previous) != count) {
            return true;
        }
        previous = count;
    }
    false
}

// Only the fields understood by the usage parser belong in the cumulative
// snapshot; arbitrary provider extensions must not grow retained state.
fn usage_update(
    raw: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut update = serde_json::Map::new();
    for (source, target) in [
        ("prompt_tokens", "prompt_tokens"),
        ("completion_tokens", "completion_tokens"),
        ("prompt_cache_hit_tokens", "prompt_cache_hit_tokens"),
        ("prompt_cache_miss_tokens", "prompt_cache_miss_tokens"),
        ("cache_read_input_tokens", "cache_read_input_tokens"),
        ("cache_creation_input_tokens", "cache_creation_input_tokens"),
        ("input_tokens", "prompt_tokens"),
        ("output_tokens", "completion_tokens"),
    ] {
        if let Some(value) = raw.get(source).filter(|value| !value.is_null()) {
            update.insert(
                target.into(),
                if value.is_number() || value.is_null() {
                    value.clone()
                } else {
                    serde_json::Value::Bool(false)
                },
            );
        }
    }
    for (first, second, target) in [
        ("input_tokens", "prompt_tokens", "prompt_tokens"),
        ("output_tokens", "completion_tokens", "completion_tokens"),
    ] {
        if invalid_aliases([raw.get(first), raw.get(second)]) {
            update.insert(target.into(), serde_json::Value::Bool(false));
        }
    }
    let details = [
        raw.get("input_tokens_details"),
        raw.get("prompt_tokens_details"),
    ];
    if details.iter().flatten().any(|value| !value.is_null()) {
        let value = if details
            .iter()
            .flatten()
            .any(|value| !value.is_null() && !value.is_object())
        {
            serde_json::Value::Bool(false)
        } else {
            let mut filtered = serde_json::Map::new();
            for key in [
                "cached_tokens",
                "cache_creation_input_tokens",
                "cache_write_tokens",
            ] {
                let values = details.map(|detail| detail.and_then(|value| value.get(key)));
                if invalid_aliases(values) {
                    filtered.insert(key.into(), serde_json::Value::Bool(false));
                } else if let Some(value) =
                    values.into_iter().flatten().find(|value| !value.is_null())
                {
                    filtered.insert(key.into(), value.clone());
                }
            }
            serde_json::Value::Object(filtered)
        };
        update.insert("prompt_tokens_details".into(), value);
    }
    update
}

type UsageSnapshot = (
    crate::turn::token_usage::TokenUsage,
    crate::turn::token_usage::TokenUsagePresence,
);

fn observe_with_usage(
    accumulator: &mut ResponseAccumulator,
    event: &serde_json::Value,
    reported: &mut serde_json::Map<String, serde_json::Value>,
    latest: &mut Option<UsageSnapshot>,
    observer: Option<(&dyn super::ProviderAttemptObserver, u32)>,
) -> Result<Option<(String, bool)>, ResponsesStreamError> {
    let observed = accumulator.observe(event);
    // The codec captures usage even when a typed failed/incomplete event
    // returns an error, but it rejects a missing event type before capture.
    if event["type"].as_str().is_some()
        && let Some(raw) = event
            .pointer("/response/usage")
            .and_then(serde_json::Value::as_object)
    {
        let previous = latest.map(|(_, presence)| presence).unwrap_or_default();
        if let Some(snapshot) =
            crate::turn::token_usage::update_openai_usage(reported, &usage_update(raw), previous)
        {
            *latest = Some(snapshot);
            if let Some((observer, attempt)) = observer {
                observer.note_usage_snapshot(attempt, snapshot.0, snapshot.1);
            }
        }
    }
    observed
}

fn result(
    output: ResponseOutput,
    connection: &str,
    model: &str,
    display_model: &str,
    latest_usage: Option<(
        crate::turn::token_usage::TokenUsage,
        crate::turn::token_usage::TokenUsagePresence,
    )>,
) -> LlmCallResult {
    let continuation = (output.finish_reason.as_deref() != Some("length"))
        .then(|| output.continuation(connection, model));
    let (usage, presence) = latest_usage
        .map(|(usage, presence)| (usage.to_qualified_json_map(presence), presence))
        .unwrap_or_else(|| normalized_usage(&output.usage));
    let finish_reason = output.finish_reason.clone().unwrap_or_else(|| {
        if output.tool_calls.is_empty() {
            "stop".to_owned()
        } else {
            "tool_calls".to_owned()
        }
    });
    LlmCallResult {
        provider_continuation: continuation,
        response_id: output.response_id,
        full_text: output.text,
        reasoning: output.reasoning,
        tool_calls: output.tool_calls,
        usage,
        usage_presence: presence,
        model_used: display_model.into(),
        finish_reason: Some(finish_reason),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn collect(
    response: reqwest::Response,
    route: LlmExecutionRoute<'_>,
    names: &HashSet<String>,
    cancel: LlmCancel<'_>,
    mut callback: Option<&mut LlmStreamCallback<'_>>,
    started: Instant,
    budget: Option<Duration>,
    lease: Option<&astra_services::chatgpt::CredentialLease>,
    usage_observer: Option<(&dyn super::ProviderAttemptObserver, u32)>,
) -> Result<LlmCallResult, StreamCollectError> {
    let binding = route.subscription;
    let scope = binding
        .map(|credential| credential.connection_id())
        .or(route.response_scope)
        .ok_or_else(|| StreamCollectError::Provider {
            error: error(
                astra_core::ErrorKind::ContractViolation,
                "response_scope_missing",
            ),
            partial: LlmCallResult::default(),
            rejected: false,
        })?;
    let mut accumulator = ResponseAccumulator::default();
    let mut reported_usage = serde_json::Map::new();
    let mut latest_usage = None;
    let mut rejected = false;
    let exchange = async {
        let mut stream = response.bytes_stream();
        let mut frames = SseBlankLineUtf8Buf::new();
        let mut size = 0;
        let mut idle_deadline = tokio::time::Instant::now() + stream_idle_timeout();
        let mut admitted_items = HashSet::new();
        loop {
            let next = tokio::select! {
                _=wait_lease_cancel(lease)=>return Err(error(astra_core::ErrorKind::Cancelled,"model_connection_unavailable")),
                _=wait_llm_cancel(cancel)=>return Err(error(astra_core::ErrorKind::Cancelled,"cancelled")),
                _=tokio::time::sleep_until(idle_deadline)=>return Err(error(astra_core::ErrorKind::ProviderDeadline,"stream_progress_deadline")),
                value=stream.next()=>value,
            };
            let eof = next.is_none();
            let blocks = if let Some(chunk) = next {
                let chunk = chunk.map_err(|_| {
                    error(
                        astra_core::ErrorKind::StreamTransport,
                        "response_interrupted",
                    )
                })?;
                size += chunk.len();
                if size > MAX_STREAM_ACCUMULATION_BYTES {
                    return Err(error(
                        astra_core::ErrorKind::ResourceLimit,
                        "stream_size_limit",
                    ));
                }
                frames.push_bytes(&chunk)
            } else {
                frames.take_buf().map(|tail| vec![tail])
            }
            .map_err(|_| {
                error(
                    astra_core::ErrorKind::StreamTransport,
                    "invalid_response_encoding",
                )
            })?;
            for block in blocks {
                validate_sse_event_block_json(&block).map_err(|_| {
                    error(
                        astra_core::ErrorKind::StreamTransport,
                        "invalid_response_event",
                    )
                })?;
                let parsed = json_events_from_sse_event_block(&block);
                for event in parsed.events {
                    if event["type"] == "response.output_item.added"
                        && event["item"]["type"] == "function_call"
                    {
                        let name = event["item"]["name"].as_str().unwrap_or("");
                        if !names.contains(name) {
                            return Err(error(
                                astra_core::ErrorKind::ContractViolation,
                                "tool_not_admitted",
                            ));
                        }
                        if let Some(id) = event["item"]["id"].as_str() {
                            admitted_items.insert(id.to_owned());
                        }
                    }
                    if event["type"] == "response.function_call_arguments.delta"
                        && event["item_id"]
                            .as_str()
                            .is_some_and(|id| admitted_items.contains(id))
                        && event["delta"]
                            .as_str()
                            .is_some_and(|delta| !delta.is_empty())
                    {
                        idle_deadline =
                            tokio::time::Instant::now() + stream_idle_timeout_after_progress();
                    }
                    let delta = observe_with_usage(
                        &mut accumulator,
                        &event,
                        &mut reported_usage,
                        &mut latest_usage,
                        usage_observer,
                    )
                    .map_err(|failure| {
                        if matches!(
                            event["type"].as_str(),
                            Some("response.failed" | "response.incomplete" | "error")
                        ) {
                            rejected = true;
                        }
                        match failure.mapped() {
                            Some((code, status)) => {
                                error(provider_http_error_kind(status, code), code)
                            }
                            None => match &failure {
                                ResponsesStreamError::Provider(_) => error(
                                    astra_core::ErrorKind::StreamTransport,
                                    "response_incomplete",
                                ),
                                ResponsesStreamError::Codec(reason) => {
                                    error(astra_core::ErrorKind::StreamTransport, reason)
                                }
                            },
                        }
                    })?;
                    if let Some((delta, reasoning)) = delta {
                        if !delta.trim().is_empty() {
                            idle_deadline =
                                tokio::time::Instant::now() + stream_idle_timeout_after_progress();
                        }
                        if let Some(callback) = callback.as_mut() {
                            callback(if reasoning {
                                LlmStreamUpdate::Reasoning(delta)
                            } else {
                                LlmStreamUpdate::Text(delta)
                            });
                        }
                    }
                    if accumulator.completed() {
                        let output = &accumulator.output;
                        if output.tool_calls.len() > MAX_STREAM_TOOL_CALLS
                            || output.tool_calls.iter().any(|call| {
                                !call["function"]["name"]
                                    .as_str()
                                    .is_some_and(|name| names.contains(name))
                            })
                        {
                            return Err(error(
                                astra_core::ErrorKind::ContractViolation,
                                "tool_not_admitted",
                            ));
                        }
                        if let Some(binding) = binding {
                            binding.ensure_current().await.map_err(credential_error)?;
                        }
                        let output = std::mem::take(&mut accumulator).finish().map_err(|_| {
                            error(
                                astra_core::ErrorKind::StreamTransport,
                                "response_completion_missing",
                            )
                        })?;
                        let result = result(
                            output,
                            scope,
                            route.wire_model_name.unwrap_or(route.model_name),
                            route.model_name,
                            latest_usage,
                        );
                        return Ok(result);
                    }
                }
            }
            if eof {
                return Err(error(
                    astra_core::ErrorKind::StreamTransport,
                    "response_completion_missing",
                ));
            }
        }
    };
    let outcome = {
        let deadline = budget.map(|budget| TokioInstant::from_std(started + budget));
        tokio::pin!(exchange);
        tokio::select! {
            biased;
            _=wait_llm_cancel(cancel)=>Err(error(astra_core::ErrorKind::Cancelled,"cancelled")),
            _=wait_lease_cancel(lease)=>Err(error(astra_core::ErrorKind::Cancelled,"model_connection_unavailable")),
            _=wait_optional_deadline(deadline)=>Err(error(astra_core::ErrorKind::ProviderDeadline,"provider_deadline")),
            value=&mut exchange=>value,
        }
    };
    outcome.map_err(|error| {
        let (usage, presence) = latest_usage
            .map(|(usage, presence)| (usage.to_qualified_json_map(presence), presence))
            .unwrap_or_else(|| normalized_usage(&accumulator.output.usage));
        StreamCollectError::Provider {
            error,
            rejected,
            partial: LlmCallResult {
                full_text: accumulator.output.text,
                reasoning: accumulator.output.reasoning,
                response_id: accumulator.output.response_id,
                usage_presence: presence,
                usage,
                model_used: route.model_name.into(),
                ..Default::default()
            },
        }
    })
}

async fn wait_lease_cancel(lease: Option<&astra_services::chatgpt::CredentialLease>) {
    match lease {
        Some(lease) => lease.cancel.cancelled().await,
        None => std::future::pending().await,
    }
}
