//! Responses execution under the ordinary inference-attempt owner.
use super::*;
use astra_core::model_wire::responses::{ResponseAccumulator, ResponseOutput};

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;

pub(super) fn error(kind: astra_core::ErrorKind, code: &str) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(kind, format!("ChatGPT subscription request failed: {code}"))
        .with_details_json(json!({"source":"chatgpt","code":code}).to_string())
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
    let credential = call.route.subscription.ok_or_else(|| {
        error(
            astra_core::ErrorKind::InvalidRequest,
            "model_connection_required",
        )
    })?;
    let fixed_base = credential.api_base().map_err(credential_error)?;
    if call.route.base_url != fixed_base || call.route.completions_url_override.is_some() {
        return Err(error(
            astra_core::ErrorKind::ContractViolation,
            "subscription_endpoint_mismatch",
        ));
    }
    if call
        .route
        .request_body_overrides
        .is_some_and(|values| !values.is_empty())
        || call.route.fixed_temperature.is_some()
    {
        return Err(error(
            astra_core::ErrorKind::InvalidRequest,
            "unsupported_subscription_generation_settings",
        ));
    }
    if !matches!(
        call.thinking,
        ThinkingConfig::ModelDefault | ThinkingConfig::Off
    ) {
        return Err(error(
            astra_core::ErrorKind::InvalidRequest,
            "unsupported_subscription_reasoning_control",
        ));
    }
    let model = call.route.wire_model_name.unwrap_or(call.route.model_name);
    // Preserve private continuation while applying the canonical advisory and
    // provider-call-ID projection. The codec validates/replays the saved items.
    let mut messages = call.messages.to_vec();
    strip_internal_runtime_markers_preserving_responses(&mut messages);
    let mut tools = call.tools.to_vec();
    for tool in &mut tools {
        strip_internal_schema_extensions(tool);
    }
    let body = astra_core::model_wire::responses::request(
        model,
        &messages,
        Some(&tools),
        credential.connection_id(),
        matches!(tool_choice, RuntimeToolChoice::None),
    )
    .map_err(|_| {
        error(
            astra_core::ErrorKind::InvalidRequest,
            "unsupported_subscription_input",
        )
    })?;
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
        None,
    )?;
    let url = format!("{}/responses", call.route.base_url.trim_end_matches('/'));
    Ok(PreparedStreamingCall {
        purpose: call.purpose,
        route: call.route,
        request,
        wire_output_limit: None,
        authorized_tool_names,
        url,
        judgment_provenance,
        tool_choice,
        preparation_elapsed: started.elapsed(),
    })
}

fn result(
    output: ResponseOutput,
    connection: &str,
    model: &str,
    display_model: &str,
) -> LlmCallResult {
    let continuation = output.continuation(connection, model);
    let presence = crate::turn::token_usage::extract_usage_presence(
        crate::turn::token_usage::UsageDialect::OpenAi,
        &output.usage,
    );
    let finish_reason = if output.tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    LlmCallResult {
        provider_continuation: Some(continuation),
        response_id: output.response_id,
        full_text: output.text,
        reasoning: output.reasoning,
        tool_calls: output.tool_calls,
        usage: output.usage,
        usage_presence: presence,
        model_used: display_model.into(),
        finish_reason: Some(finish_reason.into()),
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
    lease: &astra_services::chatgpt::CredentialLease,
) -> Result<LlmCallResult, ProviderStreamFailure> {
    let binding = route
        .subscription
        .expect("Responses preparation requires a connection");
    let mut accumulator = ResponseAccumulator::default();
    let mut rejected = false;
    let exchange = async {
        let mut stream = response.bytes_stream();
        let mut frames = SseBlankLineUtf8Buf::new();
        let mut size = 0;
        let mut idle_deadline = tokio::time::Instant::now() + stream_idle_timeout();
        let mut admitted_items = HashSet::new();
        loop {
            let next = tokio::select! {
                _=lease.cancel.cancelled()=>return Err(error(astra_core::ErrorKind::Cancelled,"model_connection_unavailable")),
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
                    let delta = accumulator.observe(&event).map_err(|code| {
                        if matches!(
                            event["type"].as_str(),
                            Some("response.failed" | "response.incomplete" | "error")
                        ) {
                            rejected = true;
                        }
                        let kind = astra_core::model_wire::responses::provider_error(&code)
                            .map(|(_, status)| provider_http_error_kind(status, &code))
                            .unwrap_or(astra_core::ErrorKind::StreamTransport);
                        error(kind, &code)
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
                        binding.ensure_current().await.map_err(credential_error)?;
                        let output = std::mem::take(&mut accumulator).finish().map_err(|_| {
                            error(
                                astra_core::ErrorKind::StreamTransport,
                                "response_completion_missing",
                            )
                        })?;
                        let result = result(
                            output,
                            binding.connection_id(),
                            route.wire_model_name.unwrap_or(route.model_name),
                            route.model_name,
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
            _=lease.cancel.cancelled()=>Err(error(astra_core::ErrorKind::Cancelled,"model_connection_unavailable")),
            _=wait_optional_deadline(deadline)=>Err(error(astra_core::ErrorKind::ProviderDeadline,"provider_deadline")),
            value=&mut exchange=>value,
        }
    };
    outcome.map_err(|error| {
        let usage = accumulator.output.usage;
        ProviderStreamFailure {
            error,
            rejected,
            partial: LlmCallResult {
                full_text: accumulator.output.text,
                reasoning: accumulator.output.reasoning,
                response_id: accumulator.output.response_id,
                usage_presence: crate::turn::token_usage::extract_usage_presence(
                    crate::turn::token_usage::UsageDialect::OpenAi,
                    &usage,
                ),
                usage,
                model_used: route.model_name.into(),
                ..Default::default()
            },
        }
    })
}
