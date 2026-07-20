//! Bedrock Converse streaming transport.
//!
//! Wires [`bedrock_eventstream::FrameDecoder`] +
//! [`bedrock_stream::BedrockStreamAccumulator`] to a real HTTP response. Two
//! entry points:
//!
//! - [`bedrock_stream_response_bytes`] — consume a `converse-stream`
//!   response and return a `Bytes` stream of canonical **internal** SSE
//!   events (`text_delta`, `reasoning_delta`, `tool_call_start`, `usage`,
//!   `_inprocess_summary`). Matches the shape produced by the OpenAI
//!   streaming path so downstream consumers don't care which provider
//!   served the turn.
//!
//! - [`collect_bedrock_stream`] — drive decoder + return a
//!   fully-aggregated [`LlmCallResult`] without producing intermediate SSE
//!   bytes. Used by [`super::llm::client::call_llm_and_collect`] so server
//!   loops get the same real-streaming behaviour as the edge.

use std::{sync::Arc, time::Instant};

use async_stream::stream;
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::turn::bedrock::eventstream::FrameDecoder;
use crate::turn::bedrock::stream::{
    BedrockStreamAccumulator, BedrockStreamEvent, RetryKind, is_retryable_exception,
};
use crate::turn::bridge::sse_helpers::render_sse;
use crate::turn::llm::client::{
    LlmCallResult, LlmCancel, LlmStreamCallback, LlmStreamUpdate, ProviderAttemptObserver,
    finish_observed_provider_attempt, finish_observed_provider_error,
    provider_attempt_terminal_from_result,
};

fn ledger_stream_error(error: &astra_core::ClassifiedError) -> Bytes {
    render_sse(&json!({
        "type": "error",
        "message": error.message,
        "code": "inference_ledger",
        "error_kind": error.kind.as_str(),
        "retryable": false,
    }))
}

fn bedrock_exception_error(kind: &str, message: &str) -> astra_core::ClassifiedError {
    let error_kind = match is_retryable_exception(kind) {
        RetryKind::RateLimit => astra_core::ErrorKind::RateLimit,
        RetryKind::Transient => astra_core::ErrorKind::ServerError,
        RetryKind::Terminal => astra_core::ErrorKind::Unknown,
    };
    astra_core::ClassifiedError::new(error_kind, format!("bedrock {kind}: {message}"))
}

async fn finish_bedrock_attempt_error(
    observer: Option<&Arc<dyn ProviderAttemptObserver>>,
    attempt_index: Option<u32>,
    error: &astra_core::ClassifiedError,
) -> Option<Bytes> {
    finish_observed_provider_error(
        observer.map(|observer| observer.as_ref()),
        attempt_index,
        error,
    )
    .await
    .err()
    .map(|error| ledger_stream_error(&error))
}

async fn finish_bedrock_attempt_success(
    observer: Option<&Arc<dyn ProviderAttemptObserver>>,
    attempt_index: Option<u32>,
    result: &LlmCallResult,
) -> Option<Bytes> {
    finish_observed_provider_attempt(
        observer.map(|observer| observer.as_ref()),
        attempt_index,
        &provider_attempt_terminal_from_result(result),
    )
    .await
    .err()
    .map(|error| ledger_stream_error(&error))
}

/// Public error returned by [`collect_bedrock_stream`]. Every non-success
/// carries the accumulator snapshot so callers can preserve already observed
/// evidence without silently reissuing the inference request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BedrockStreamError {
    #[error("bedrock transport error: {error}")]
    Transport {
        error: String,
        partial: LlmCallResult,
    },
    #[error("bedrock stream aborted by cancel")]
    Cancelled { partial: LlmCallResult },
    #[error("bedrock exception frame: {kind} — {message}")]
    Exception {
        kind: String,
        message: String,
        partial: LlmCallResult,
    },
}

/// Consume a `converse-stream` response body and return a `Bytes` stream of
/// our canonical internal SSE events. A successful stream ends with
/// `_inprocess_summary`; cancellation, transport, parsing, and provider
/// exception paths end with a typed error and never fabricate a summary.
///
/// This function **returns** a stream — it does not drive polling itself.
pub(crate) fn bedrock_stream_response_bytes(
    response: reqwest::Response,
    model_name: String,
    started: Instant,
    cancel: Option<std::sync::Arc<CancellationToken>>,
    idle_timeout: std::time::Duration,
    attempt_observer: Option<Arc<dyn ProviderAttemptObserver>>,
    observed_attempt: Option<u32>,
) -> impl futures_util::Stream<Item = Bytes> + Send + 'static {
    stream! {
        let mut decoder = FrameDecoder::new();
        let mut accum = BedrockStreamAccumulator::new();
        let mut byte_stream = response.bytes_stream();

        'body: loop {
            // Cancellation wins over everything.
            if let Some(ct) = cancel.as_ref()
                && ct.is_cancelled()
            {
                if accum.has_message_stop() {
                    break;
                }
                let error = astra_core::ClassifiedError::new(
                    astra_core::ErrorKind::StreamTransport,
                    "Bedrock stream delivery became unknown after client disconnect",
                );
                if let Some(ledger_error) = finish_bedrock_attempt_error(
                    attempt_observer.as_ref(),
                    observed_attempt,
                    &error,
                ).await {
                    yield ledger_error;
                    return;
                }
                yield render_sse(&json!({
                    "type": "error",
                    "message": error.message,
                    "code": "client_disconnect",
                    "error_kind": error.kind.as_str(),
                    "retryable": false,
                }));
                return;
            }

            let idle = if accum.has_message_stop() {
                crate::turn::llm::client::stream_terminal_drain_timeout(idle_timeout)
            } else {
                idle_timeout
            };
            let next = tokio::time::timeout(idle, byte_stream.next()).await;
            let chunk_result = match next {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_elapsed) if accum.has_message_stop() => break,
                Err(_elapsed) => {
                    let error = astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::StreamIdle,
                        format!("bedrock stream idle > {}ms", idle.as_millis()),
                    );
                    if let Some(ledger_error) = finish_bedrock_attempt_error(
                        attempt_observer.as_ref(),
                        observed_attempt,
                        &error,
                    ).await {
                        yield ledger_error;
                        return;
                    }
                    yield render_sse(&json!({
                        "type": "error",
                        "message": error.message,
                        "code": "stream_idle",
                        "error_kind": error.kind.as_str(),
                        "retryable": false,
                    }));
                    return;
                }
            };
            let chunk = match chunk_result {
                Ok(b) => b,
                Err(_error) if accum.has_message_stop() => break,
                Err(e) => {
                    let error = astra_core::ClassifiedError::new(
                        astra_core::ErrorKind::StreamTransport,
                        format!("bedrock transport error: {e}"),
                    );
                    if let Some(ledger_error) = finish_bedrock_attempt_error(
                        attempt_observer.as_ref(),
                        observed_attempt,
                        &error,
                    ).await {
                        yield ledger_error;
                        return;
                    }
                    yield render_sse(&json!({
                        "type": "error",
                        "message": error.message,
                        "code": "stream_transport",
                        "error_kind": error.kind.as_str(),
                        "retryable": false,
                    }));
                    return;
                }
            };

            decoder.push(&chunk);

            loop {
                match decoder.try_next_frame() {
                    Ok(Some(frame)) => {
                        match accum.push_frame(&frame) {
                            Ok(events) => {
                                for ev in events {
                                    if let BedrockStreamEvent::Exception { kind, message } = &ev {
                                        let error = bedrock_exception_error(kind, message);
                                        if let Some(ledger_error) = finish_bedrock_attempt_error(
                                            attempt_observer.as_ref(),
                                            observed_attempt,
                                            &error,
                                        ).await {
                                            yield ledger_error;
                                            return;
                                        }
                                        for bytes in canonical_event_bytes(&ev) {
                                            yield bytes;
                                        }
                                        return;
                                    }
                                    for bytes in canonical_event_bytes(&ev) {
                                        yield bytes;
                                    }
                                }
                            }
                            Err(e) => {
                                if accum.has_message_stop() {
                                    break 'body;
                                }
                                let error = astra_core::ClassifiedError::new(
                                    astra_core::ErrorKind::StreamTransport,
                                    format!("bedrock frame parse: {e}"),
                                );
                                if let Some(ledger_error) = finish_bedrock_attempt_error(
                                    attempt_observer.as_ref(),
                                    observed_attempt,
                                    &error,
                                ).await {
                                    yield ledger_error;
                                    return;
                                }
                                yield render_sse(&json!({
                                    "type": "error",
                                    "message": error.message,
                                    "code": "bedrock_frame",
                                    "error_kind": error.kind.as_str(),
                                    "retryable": false,
                                }));
                                return;
                            }
                        }
                    }
                    Ok(None) => break, // need more bytes
                    Err(e) => {
                        if accum.has_message_stop() {
                            break 'body;
                        }
                        let error = astra_core::ClassifiedError::new(
                            astra_core::ErrorKind::StreamTransport,
                            format!("bedrock eventstream decode: {e}"),
                        );
                        if let Some(ledger_error) = finish_bedrock_attempt_error(
                            attempt_observer.as_ref(),
                            observed_attempt,
                            &error,
                        ).await {
                            yield ledger_error;
                            return;
                        }
                        yield render_sse(&json!({
                            "type": "error",
                            "message": error.message,
                            "code": "bedrock_decode",
                            "error_kind": error.kind.as_str(),
                            "retryable": false,
                        }));
                        return;
                    }
                }
            }

            // Do not break on `messageStop`: Bedrock Converse can deliver
            // Bedrock Converse delivers the `metadata` frame (carrying
            // usage) afterward. Drain to EOS, but bound the terminal tail by
            // a short grace period so a broken keepalive cannot stall finish.
        }

        if !accum.has_message_stop() {
            let error = astra_core::ClassifiedError::new(
                astra_core::ErrorKind::StreamTransport,
                "Bedrock stream ended without messageStop",
            );
            if let Some(ledger_error) = finish_bedrock_attempt_error(
                attempt_observer.as_ref(),
                observed_attempt,
                &error,
            ).await {
                yield ledger_error;
                return;
            }
            yield render_sse(&json!({
                "type": "error",
                "message": error.message,
                "code": "stream_transport",
                "error_kind": error.kind.as_str(),
                "retryable": false,
            }));
            return;
        }

        // Emit the terminal _inprocess_summary carrying the aggregated
        // LlmCallResult, matching the OpenAI-path contract.
        let result = accum.into_result(&model_name, started.elapsed().as_millis() as u64);
        if let Some(ledger_error) = finish_bedrock_attempt_success(
            attempt_observer.as_ref(),
            observed_attempt,
            &result,
        ).await {
            yield ledger_error;
            return;
        }
        yield render_sse(&json!({
            "type": "_inprocess_summary",
            "full_text": result.full_text,
            "reasoning": result.reasoning,
            "reasoning_signature": result.reasoning_signature,
            "tool_calls": result.tool_calls,
            "usage": result.usage,
            "model_used": result.model_used,
            "provider_response_id": result.response_id,
        }));
    }
}

/// Convert one canonical [`BedrockStreamEvent`] into the matching internal
/// SSE byte frame(s). Usage is emitted with the same canonical keys the
/// OpenAI path uses (see [`crate::turn::token_usage::TokenUsage`]).
fn canonical_event_bytes(ev: &BedrockStreamEvent) -> Vec<Bytes> {
    match ev {
        BedrockStreamEvent::TextDelta(text) => {
            vec![render_sse(&json!({
                "type": "text_delta",
                "content": text,
            }))]
        }
        BedrockStreamEvent::ReasoningDelta(text) => {
            vec![render_sse(&json!({
                "type": "reasoning_delta",
                "content": text,
            }))]
        }
        BedrockStreamEvent::ToolCallStart { id, name } => {
            vec![render_sse(&json!({
                "type": "tool_call_start",
                "tool": name,
                "call_id": id,
            }))]
        }
        BedrockStreamEvent::ToolCallDelta { .. } => vec![],
        BedrockStreamEvent::Usage(u) => {
            vec![render_sse(&json!({
                "type": "usage",
                "input_tokens": u.input_tokens,
                "cached_input_tokens": u.cached_input_tokens,
                "cache_creation_tokens": u.cache_creation_tokens,
                "output_tokens": u.output_tokens,
                "total_tokens": u.total_tokens(),
            }))]
        }
        BedrockStreamEvent::MessageStop { .. } => vec![],
        BedrockStreamEvent::Exception { kind, message } => {
            let error = bedrock_exception_error(kind, message);
            vec![render_sse(&json!({
                "type": "error",
                "message": error.message,
                "code": kind,
                "error_kind": error.kind.as_str(),
                "retryable": is_retryable_exception(kind).is_retryable(),
            }))]
        }
    }
}

/// Drive a `converse-stream` HTTP response to completion and return an
/// aggregated [`LlmCallResult`]. No intermediate SSE bytes are produced.
///
/// `idle_timeout` bounds the per-chunk wait. If the server goes quiet for
/// longer the call aborts with [`BedrockStreamError::Transport`]. Because
/// provider delivery is then uncertain, the caller must not reissue it.
pub(crate) async fn collect_bedrock_stream(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, BedrockStreamError> {
    let mut decoder = FrameDecoder::new();
    let mut accum = BedrockStreamAccumulator::new();
    let mut byte_stream = response.bytes_stream();
    let partial = |accum: &BedrockStreamAccumulator| {
        accum
            .clone()
            .into_result(model_name, started.elapsed().as_millis() as u64)
    };

    'body: loop {
        if cancel.is_triggered() {
            if accum.has_message_stop() {
                break;
            }
            return Err(BedrockStreamError::Cancelled {
                partial: partial(&accum),
            });
        }

        let idle = if accum.has_message_stop() {
            crate::turn::llm::client::stream_terminal_drain_timeout(idle_timeout)
        } else {
            idle_timeout
        };
        let next = tokio::time::timeout(idle, byte_stream.next()).await;
        let chunk_result = match next {
            Ok(Some(c)) => c,
            Ok(None) => break, // end of stream
            Err(_elapsed) if accum.has_message_stop() => break,
            Err(_elapsed) => {
                return Err(BedrockStreamError::Transport {
                    error: format!("bedrock stream idle > {}ms", idle.as_millis()),
                    partial: partial(&accum),
                });
            }
        };

        let chunk = match chunk_result {
            Ok(chunk) => chunk,
            Err(_error) if accum.has_message_stop() => break,
            Err(error) => {
                return Err(BedrockStreamError::Transport {
                    error: error.to_string(),
                    partial: partial(&accum),
                });
            }
        };
        decoder.push(&chunk);

        loop {
            let frame = match decoder.try_next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_error) if accum.has_message_stop() => break 'body,
                Err(error) => {
                    return Err(BedrockStreamError::Transport {
                        error: error.to_string(),
                        partial: partial(&accum),
                    });
                }
            };
            let events = match accum.push_frame(&frame) {
                Ok(events) => events,
                Err(_error) if accum.has_message_stop() => break 'body,
                Err(error) => {
                    return Err(BedrockStreamError::Transport {
                        error: error.to_string(),
                        partial: partial(&accum),
                    });
                }
            };
            for ev in events {
                match &ev {
                    BedrockStreamEvent::TextDelta(text) => {
                        if let Some(callback) = stream_callback.as_deref_mut() {
                            callback(LlmStreamUpdate::Text(text.clone()));
                        }
                    }
                    BedrockStreamEvent::ReasoningDelta(text) => {
                        if let Some(callback) = stream_callback.as_deref_mut() {
                            callback(LlmStreamUpdate::Reasoning(text.clone()));
                        }
                    }
                    BedrockStreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        if let Some(callback) = stream_callback.as_deref_mut() {
                            callback(LlmStreamUpdate::ToolCall {
                                index: *index as usize,
                                tool_call: json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": arguments,
                                    }
                                }),
                            });
                        }
                    }
                    _ => {}
                }
                if let BedrockStreamEvent::Exception { kind, message } = ev {
                    return Err(BedrockStreamError::Exception {
                        kind,
                        message,
                        partial: partial(&accum),
                    });
                }
            }
        }

        // See [`bedrock_stream_response_bytes`]: drain metadata after
        // `messageStop`, bounded by the short terminal-tail grace period.
    }

    if !accum.has_message_stop() {
        return Err(BedrockStreamError::Transport {
            error: "bedrock stream ended without messageStop".to_string(),
            partial: partial(&accum),
        });
    }

    Ok(accum.into_result(model_name, started.elapsed().as_millis() as u64))
}
