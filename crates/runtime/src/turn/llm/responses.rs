//! Responses execution under the ordinary inference-attempt owner.
use super::*;
use astra_core::model_wire::responses::{ResponseAccumulator, ResponseOutput};

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;

fn error(kind: astra_core::ErrorKind, code: &str) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(kind, format!("ChatGPT subscription request failed: {code}"))
        .with_details_json(json!({"source":"chatgpt","code":code}).to_string())
}
fn credential_error(
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
    LlmCallResult {
        provider_continuation: Some(continuation),
        response_id: output.response_id,
        full_text: output.text,
        reasoning: output.reasoning,
        tool_calls: output.tool_calls.clone(),
        usage: output.usage,
        usage_presence: presence,
        model_used: display_model.into(),
        finish_reason: Some(
            if output.tool_calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
        ),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    route: LlmExecutionRoute<'_>,
    request: PreparedProviderRequest,
    url: &str,
    names: HashSet<String>,
    provenance: Option<astra_turn_types::JudgmentResponseProvenance>,
    cancel: LlmCancel<'_>,
    mut callback: Option<&mut LlmStreamCallback<'_>>,
    observer: Option<&dyn ProviderAttemptObserver>,
    started: Instant,
    budget: Duration,
) -> Result<LlmCallResult, astra_core::ClassifiedError> {
    let binding = route.subscription.ok_or_else(|| {
        error(
            astra_core::ErrorKind::InvalidRequest,
            "model_connection_required",
        )
    })?;
    let mut rejected_version = None;
    let mut auth_retried = false;
    let client = binding.http_client().map_err(credential_error)?;
    for attempt in 0..=LLM_MAX_RETRIES {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(error(
                astra_core::ErrorKind::ProviderDeadline,
                "provider_deadline",
            ));
        }
        let lease = tokio::select! {
            _=wait_llm_cancel(cancel)=>return Err(error(astra_core::ErrorKind::Cancelled,"cancelled")),
            value=tokio::time::timeout(remaining,binding.acquire(rejected_version.take()))=>value.map_err(|_| error(astra_core::ErrorKind::ProviderDeadline,"credential_deadline"))?.map_err(credential_error)?,
        };
        let index = begin_observed_provider_attempt(observer, request.identity()).await?;
        let mut partial = LlmCallResult {
            model_used: route.model_name.into(),
            ..Default::default()
        };
        let mut accumulator = ResponseAccumulator::default();
        let mut rejected = false;
        let mut retry = false;
        let mut dispatched = false;
        let exchange = async {
            if let (Some(observer), Some(index)) = (observer, index) {
                observer.note_dispatch_started(index);
            }
            dispatched = true;
            let response = client
                .post(url)
                .bearer_auth(lease.bearer())
                .header("content-type", "application/json")
                .body(request.body())
                .timeout(route.request_timeout.unwrap_or(remaining).min(remaining))
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(failure) => {
                    rejected = failure.is_connect();
                    retry = rejected;
                    return Err(error(
                        if failure.is_timeout() {
                            astra_core::ErrorKind::ProviderDeadline
                        } else {
                            astra_core::ErrorKind::StreamTransport
                        },
                        "provider_transport_failed",
                    ));
                }
            };
            let status = response.status();
            if !status.is_success() {
                rejected = true;
                if status.as_u16() == 401 && !auth_retried {
                    rejected_version = Some(lease.version());
                    auth_retried = true;
                    retry = true;
                } else if status.is_server_error() {
                    retry = true;
                }
                let mut response = response;
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| {
                    error(
                        astra_core::ErrorKind::StreamTransport,
                        "provider_error_interrupted",
                    )
                })? {
                    if bytes.len() + chunk.len() > 1024 * 1024 {
                        return Err(error(
                            astra_core::ErrorKind::ResourceLimit,
                            "provider_error_size_limit",
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let code = astra_services::chatgpt::provider_error_code(status.as_u16(), &body);
                return Err(error(provider_http_error_kind(status.as_u16(), code), code)
                    .with_details_json(
                        json!({"source":"chatgpt","code":code,"http_status":status.as_u16()})
                            .to_string(),
                    ));
            }
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
                                idle_deadline = tokio::time::Instant::now()
                                    + stream_idle_timeout_after_progress();
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
                            let output =
                                std::mem::take(&mut accumulator).finish().map_err(|_| {
                                    error(
                                        astra_core::ErrorKind::StreamTransport,
                                        "response_completion_missing",
                                    )
                                })?;
                            let mut result = result(
                                output,
                                binding.connection_id(),
                                route.wire_model_name.unwrap_or(route.model_name),
                                route.model_name,
                            );
                            result.judgment_provenance = provenance;
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
            let timed = tokio::time::timeout(budget.saturating_sub(started.elapsed()), exchange);
            tokio::pin!(timed);
            tokio::select! {
                biased;
                _=wait_llm_cancel(cancel)=>Err(error(astra_core::ErrorKind::Cancelled,"cancelled")),
                _=lease.cancel.cancelled()=>Err(error(astra_core::ErrorKind::Cancelled,"model_connection_unavailable")),
                value=&mut timed=>value.unwrap_or_else(|_|Err(error(astra_core::ErrorKind::ProviderDeadline,"provider_deadline"))),
            }
        };
        match outcome {
            Ok(result) => {
                note_observed_provider_usage_presence(observer, index, result.usage_presence);
                finish_observed_provider_attempt(
                    observer,
                    index,
                    &provider_attempt_terminal_from_result(&result),
                )
                .await?;
                return Ok(result);
            }
            Err(failure) => {
                // Move a partial snapshot once at settlement, rather than
                // cloning accumulated history on every streamed delta.
                partial.full_text = std::mem::take(&mut accumulator.output.text);
                partial.reasoning = std::mem::take(&mut accumulator.output.reasoning);
                partial.response_id = accumulator.output.response_id.take();
                partial.usage = std::mem::take(&mut accumulator.output.usage);
                partial.usage_presence = crate::turn::token_usage::extract_usage_presence(
                    crate::turn::token_usage::UsageDialect::OpenAi,
                    &partial.usage,
                );
                if rejected || !dispatched {
                    finish_observed_provider_error_with_partial(
                        observer, index, &failure, &partial,
                    )
                    .await?;
                } else {
                    finish_observed_provider_delivery_unknown_with_partial(
                        observer, index, &failure, &partial,
                    )
                    .await?;
                }
                if !retry || attempt == LLM_MAX_RETRIES {
                    return Err(attach_llm_result_details(failure, &partial));
                }
                if rejected_version.is_none() {
                    wait_provider_retry(retry_backoff_ms(attempt + 1), cancel, started, budget)
                        .await?;
                }
            }
        }
    }
    Err(error(
        astra_core::ErrorKind::ProviderDeadline,
        "provider_deadline",
    ))
}
