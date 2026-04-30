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
//! - [`call_bedrock_and_collect`] — POST + drive decoder + return a
//!   fully-aggregated [`LlmCallResult`] without producing intermediate SSE
//!   bytes. Used by [`super::llm_client::call_llm_and_collect`] so server
//!   loops get the same real-streaming behaviour as the edge.

use std::time::Instant;

use async_stream::stream;
use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::turn::bedrock_eventstream::FrameDecoder;
use crate::turn::bedrock_stream::{BedrockStreamAccumulator, BedrockStreamEvent};
use crate::turn::bridge_sse_helpers::render_sse;
use crate::turn::llm_client::{LlmCallResult, LlmCancel};

/// Public error returned by [`call_bedrock_and_collect`]. Mirrors the shape
/// of [`crate::turn::llm_client::StreamCollectError`] but without the
/// partial-result carrying — the accumulator preserves partials directly.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BedrockStreamError {
    #[error("bedrock transport error: {0}")]
    Transport(String),
    #[error("bedrock stream aborted by cancel")]
    Cancelled,
    #[error("bedrock exception frame: {kind} — {message}")]
    Exception { kind: String, message: String },
}

/// Consume a `converse-stream` response body and return a `Bytes` stream of
/// our canonical internal SSE events. The final event is always
/// `_inprocess_summary` so downstream can rebuild [`LlmCallResult`].
///
/// This function **returns** a stream — it does not drive polling itself.
pub(crate) fn bedrock_stream_response_bytes(
    response: reqwest::Response,
    model_name: String,
    started: Instant,
    cancel: Option<std::sync::Arc<CancellationToken>>,
    idle_timeout: std::time::Duration,
) -> impl futures_util::Stream<Item = Bytes> + Send + 'static {
    stream! {
        let mut decoder = FrameDecoder::new();
        let mut accum = BedrockStreamAccumulator::new();
        let mut byte_stream = response.bytes_stream();

        loop {
            // Cancellation wins over everything.
            if let Some(ct) = cancel.as_ref()
                && ct.is_cancelled()
            {
                yield render_sse(&json!({
                    "type": "error",
                    "message": "bedrock stream cancelled",
                    "code": "cancelled",
                    "retryable": false,
                }));
                break;
            }

            let next = tokio::time::timeout(idle_timeout, byte_stream.next()).await;
            let chunk_result = match next {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(_elapsed) => {
                    yield render_sse(&json!({
                        "type": "error",
                        "message": format!("bedrock stream idle > {}ms", idle_timeout.as_millis()),
                        "code": "stream_idle",
                        "retryable": true,
                    }));
                    break;
                }
            };
            let chunk = match chunk_result {
                Ok(b) => b,
                Err(e) => {
                    yield render_sse(&json!({
                        "type": "error",
                        "message": format!("bedrock transport error: {e}"),
                        "code": "stream_transport",
                        "retryable": true,
                    }));
                    break;
                }
            };

            decoder.push(&chunk);

            loop {
                match decoder.try_next_frame() {
                    Ok(Some(frame)) => {
                        match accum.push_frame(&frame) {
                            Ok(events) => {
                                for ev in events {
                                    for bytes in canonical_event_bytes(&ev) {
                                        yield bytes;
                                    }
                                }
                            }
                            Err(e) => {
                                yield render_sse(&json!({
                                    "type": "error",
                                    "message": format!("bedrock frame parse: {e}"),
                                    "code": "bedrock_frame",
                                    "retryable": false,
                                }));
                                return;
                            }
                        }
                    }
                    Ok(None) => break, // need more bytes
                    Err(e) => {
                        yield render_sse(&json!({
                            "type": "error",
                            "message": format!("bedrock eventstream decode: {e}"),
                            "code": "bedrock_decode",
                            "retryable": false,
                        }));
                        return;
                    }
                }
            }

            // Do NOT break on `accum.is_finished()` after `messageStop` —
            // Bedrock Converse delivers the `metadata` frame (carrying
            // usage) AFTER `messageStop`, often in a separate TCP chunk.
            // Draining until EOS is the only way to capture usage.
            // Exceptions are the one exception: they are truly terminal.
            if accum.has_exception() {
                break;
            }
        }

        // Emit the terminal _inprocess_summary carrying the aggregated
        // LlmCallResult, matching the OpenAI-path contract.
        let result = accum.into_result(&model_name, started.elapsed().as_millis() as u64);
        yield render_sse(&json!({
            "type": "_inprocess_summary",
            "full_text": result.full_text,
            "reasoning": result.reasoning,
            "reasoning_signature": result.reasoning_signature,
            "tool_calls": result.tool_calls,
            "usage": result.usage,
            "model_used": result.model_used,
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
            use crate::turn::bedrock_stream::retryable_exception;
            vec![render_sse(&json!({
                "type": "error",
                "message": format!("bedrock {kind}: {message}"),
                "code": kind,
                "retryable": retryable_exception(kind),
            }))]
        }
    }
}

/// Drive a `converse-stream` HTTP response to completion and return an
/// aggregated [`LlmCallResult`]. No intermediate SSE bytes are produced.
///
/// `idle_timeout` bounds the per-chunk wait. If the server goes quiet for
/// longer the call aborts with [`BedrockStreamError::Transport`] so the
/// retry loop in `call_llm_and_collect` can react.
pub(crate) async fn collect_bedrock_stream(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
) -> Result<LlmCallResult, BedrockStreamError> {
    let mut decoder = FrameDecoder::new();
    let mut accum = BedrockStreamAccumulator::new();
    let mut byte_stream = response.bytes_stream();

    loop {
        if cancel.is_triggered() {
            return Err(BedrockStreamError::Cancelled);
        }

        let next = tokio::time::timeout(idle_timeout, byte_stream.next()).await;
        let chunk_result = match next {
            Ok(Some(c)) => c,
            Ok(None) => break, // end of stream
            Err(_elapsed) => {
                return Err(BedrockStreamError::Transport(format!(
                    "bedrock stream idle > {}ms",
                    idle_timeout.as_millis()
                )));
            }
        };

        let chunk = chunk_result.map_err(|e| BedrockStreamError::Transport(e.to_string()))?;
        decoder.push(&chunk);

        while let Some(frame) = decoder
            .try_next_frame()
            .map_err(|e| BedrockStreamError::Transport(e.to_string()))?
        {
            let events = accum
                .push_frame(&frame)
                .map_err(|e| BedrockStreamError::Transport(e.to_string()))?;
            for ev in events {
                if let BedrockStreamEvent::Exception { kind, message } = ev {
                    return Err(BedrockStreamError::Exception { kind, message });
                }
            }
        }

        // See [`bedrock_stream_response_bytes`] — `metadata` (usage) arrives
        // AFTER `messageStop`, so we drain until EOS. Only a true terminal
        // (exception) justifies an early exit.
        if accum.has_exception() {
            break;
        }
    }

    Ok(accum.into_result(model_name, started.elapsed().as_millis() as u64))
}
