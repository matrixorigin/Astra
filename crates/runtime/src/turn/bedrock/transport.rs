//! Bedrock Converse streaming transport.
//!
//! The server-owned loop consumes provider streams as typed results while
//! forwarding live deltas through a callback. There is no parallel SSE bridge
//! transport or client-owned continuation path.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use futures_util::StreamExt;
use serde_json::json;
use tokio::time::Instant as TokioInstant;

use crate::turn::bedrock::eventstream::FrameDecoder;
use crate::turn::bedrock::stream::{BedrockStreamAccumulator, BedrockStreamEvent};
use crate::turn::llm::client::{
    LlmCallResult, LlmCancel, LlmStreamCallback, LlmStreamUpdate, StreamYieldState,
};

fn bedrock_response_id(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Every non-success carries the accumulator snapshot so callers preserve
/// already observed evidence without silently reissuing an inference request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BedrockStreamError {
    #[error("bedrock transport error: {error}")]
    Transport {
        error: String,
        partial: LlmCallResult,
    },
    #[error("bedrock stream aborted by cancel")]
    Cancelled { partial: LlmCallResult },
    #[error("bedrock stream produced no semantic progress for {elapsed_ms}ms")]
    SemanticProgressTimeout {
        elapsed_ms: u64,
        made_semantic_progress: bool,
        partial: LlmCallResult,
    },
    #[error("bedrock stream exceeded the provider work budget after {elapsed_ms}ms")]
    ProviderWorkDeadline {
        elapsed_ms: u64,
        partial: LlmCallResult,
    },
    #[error("bedrock exception frame: {kind} — {message}")]
    Exception {
        kind: String,
        message: String,
        partial: LlmCallResult,
    },
}

/// Drive a `converse-stream` HTTP response to completion and return an
/// aggregated [`LlmCallResult`]. No intermediate SSE bytes are produced.
///
/// `idle_timeout` bounds the per-chunk wait. If the server goes quiet for
/// longer the call aborts with [`BedrockStreamError::Transport`]. Because
/// provider delivery is then uncertain, the caller must not reissue it.
#[cfg(test)]
pub(crate) async fn collect_bedrock_stream(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
    stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, BedrockStreamError> {
    collect_bedrock_stream_with_semantic_progress_deadline(
        response,
        model_name,
        started,
        cancel,
        idle_timeout,
        crate::turn::llm::client::llm_semantic_progress_timeout(),
        stream_callback,
    )
    .await
}

pub(crate) async fn collect_bedrock_stream_for_wire(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    provider_work_budget: std::time::Duration,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
    authorized_tool_names: &HashSet<String>,
    stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, BedrockStreamError> {
    collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
        response,
        model_name,
        started,
        provider_work_budget,
        cancel,
        idle_timeout,
        crate::turn::llm::client::llm_semantic_progress_timeout(),
        Some(authorized_tool_names),
        stream_callback,
    )
    .await
}

#[cfg(test)]
async fn collect_bedrock_stream_with_semantic_progress_deadline(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
    semantic_progress_timeout: std::time::Duration,
    stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, BedrockStreamError> {
    collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
        response,
        model_name,
        started,
        crate::turn::llm::client::llm_total_budget(),
        cancel,
        idle_timeout,
        semantic_progress_timeout,
        None,
        stream_callback,
    )
    .await
}

async fn collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
    response: reqwest::Response,
    model_name: &str,
    started: Instant,
    provider_work_budget: std::time::Duration,
    cancel: LlmCancel<'_>,
    idle_timeout: std::time::Duration,
    semantic_progress_timeout: std::time::Duration,
    authorized_tool_names: Option<&HashSet<String>>,
    mut stream_callback: Option<&mut LlmStreamCallback<'_>>,
) -> Result<LlmCallResult, BedrockStreamError> {
    let mut decoder = FrameDecoder::new();
    let mut accum = BedrockStreamAccumulator::new();
    accum
        .set_provider_response_id(bedrock_response_id(&response))
        .map_err(|error| BedrockStreamError::Transport {
            error: error.to_string(),
            partial: accum
                .clone()
                .into_result(model_name, started.elapsed().as_millis() as u64),
        })?;
    let mut byte_stream = response.bytes_stream();
    let mut yield_state = StreamYieldState::new(TokioInstant::now());
    let mut delivered_tool_arguments = HashMap::<u64, String>::new();
    let provider_work_deadline = started + provider_work_budget;
    let partial = |accum: &BedrockStreamAccumulator| {
        accum
            .clone()
            .into_result(model_name, started.elapsed().as_millis() as u64)
    };

    'body: loop {
        if cancel.is_triggered() {
            if accum.has_complete_terminal_facts() {
                break;
            }
            return Err(BedrockStreamError::Cancelled {
                partial: partial(&accum),
            });
        }
        if !accum.has_complete_terminal_facts() && started.elapsed() >= provider_work_budget {
            return Err(BedrockStreamError::ProviderWorkDeadline {
                elapsed_ms: provider_work_budget.as_millis() as u64,
                partial: partial(&accum),
            });
        }
        if yield_state.timed_out(TokioInstant::now(), semantic_progress_timeout) {
            return Err(BedrockStreamError::SemanticProgressTimeout {
                elapsed_ms: semantic_progress_timeout.as_millis() as u64,
                made_semantic_progress: yield_state.has_actionable_yield(),
                partial: partial(&accum),
            });
        }

        let idle = if accum.has_message_stop() {
            crate::turn::llm::client::stream_terminal_drain_timeout(idle_timeout)
        } else {
            idle_timeout
        };
        let yield_deadline = yield_state
            .deadline(semantic_progress_timeout)
            .unwrap_or_else(|| TokioInstant::from_std(provider_work_deadline));
        let next = tokio::select! {
            biased;
            _ = crate::turn::llm::client::wait_llm_cancel(cancel) => {
                if accum.has_complete_terminal_facts() {
                    break;
                }
                return Err(BedrockStreamError::Cancelled {
                    partial: partial(&accum),
                });
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                provider_work_deadline
            )), if !accum.has_complete_terminal_facts() => {
                return Err(BedrockStreamError::ProviderWorkDeadline {
                    elapsed_ms: provider_work_budget.as_millis() as u64,
                    partial: partial(&accum),
                });
            },
            _ = tokio::time::sleep_until(yield_deadline), if !yield_state.is_terminal() => {
                return Err(BedrockStreamError::SemanticProgressTimeout {
                    elapsed_ms: semantic_progress_timeout.as_millis() as u64,
                    made_semantic_progress: yield_state.has_actionable_yield(),
                    partial: partial(&accum),
                });
            },
            next = tokio::time::timeout(idle, byte_stream.next()) => next,
        };
        let chunk_result = match next {
            Ok(Some(c)) => c,
            Ok(None) => break, // end of stream
            Err(_elapsed) if accum.has_complete_terminal_facts() => break,
            Err(_elapsed) => {
                return Err(BedrockStreamError::Transport {
                    error: format!("bedrock stream idle > {}ms", idle.as_millis()),
                    partial: partial(&accum),
                });
            }
        };

        let chunk = match chunk_result {
            Ok(chunk) => chunk,
            Err(_error) if accum.has_complete_terminal_facts() => break,
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
                Err(_error) if accum.has_complete_terminal_facts() => break 'body,
                Err(error) => {
                    return Err(BedrockStreamError::Transport {
                        error: error.to_string(),
                        partial: partial(&accum),
                    });
                }
            };
            let events = match accum.push_frame(&frame) {
                Ok(events) => events,
                Err(_error) if accum.has_complete_terminal_facts() => break 'body,
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
                        yield_state.observe_text(text, TokioInstant::now());
                        if let Some(callback) = stream_callback.as_deref_mut() {
                            callback(LlmStreamUpdate::Text(text.clone()));
                        }
                    }
                    BedrockStreamEvent::ReasoningDelta(text) => {
                        yield_state.observe_reasoning_activity(text, TokioInstant::now());
                        if let Some(callback) = stream_callback.as_deref_mut() {
                            callback(LlmStreamUpdate::Reasoning(text.clone()));
                        }
                    }
                    BedrockStreamEvent::ToolCallStart { name, .. } => {
                        yield_state.observe_tool_delivery(
                            name,
                            authorized_tool_names,
                            false,
                            TokioInstant::now(),
                        );
                    }
                    BedrockStreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        let fragment_advanced = !arguments.is_empty()
                            && delivered_tool_arguments
                                .get(index)
                                .is_none_or(|previous| previous != arguments);
                        delivered_tool_arguments.insert(*index, arguments.clone());
                        yield_state.observe_tool_delivery(
                            name,
                            authorized_tool_names,
                            fragment_advanced,
                            TokioInstant::now(),
                        );
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
                    BedrockStreamEvent::MessageStop { .. } => yield_state.mark_terminal(),
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

        // Drain metadata after `messageStop`, bounded by the short
        // terminal-tail grace period.
    }

    if !accum.has_complete_terminal_facts() {
        let missing_fact = if accum.has_message_stop() {
            "trailing usage metadata"
        } else {
            "messageStop"
        };
        return Err(BedrockStreamError::Transport {
            error: format!("bedrock stream ended without {missing_fact}"),
            partial: partial(&accum),
        });
    }

    Ok(accum.into_result(model_name, started.elapsed().as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn eventstream_frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        fn string_header(out: &mut Vec<u8>, name: &str, value: &str) {
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
            out.push(7);
            out.extend_from_slice(&(value.len() as u16).to_be_bytes());
            out.extend_from_slice(value.as_bytes());
        }

        let mut headers = Vec::new();
        string_header(&mut headers, ":message-type", "event");
        string_header(&mut headers, ":event-type", event_type);
        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;
        let mut frame = Vec::with_capacity(total_len as usize);
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&headers_len.to_be_bytes());
        frame.extend_from_slice(&crc32fast::hash(&frame[..8]).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
        frame
    }

    async fn spawn_bedrock_stream(include_metadata: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Bedrock fixture");
        let address = listener.local_addr().expect("Bedrock fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept Bedrock request");
            socket.set_nodelay(true).expect("disable Nagle");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;

            let mut terminal_prefix = eventstream_frame("messageStart", br#"{"role":"assistant"}"#);
            terminal_prefix.extend(eventstream_frame(
                "contentBlockDelta",
                br#"{"contentBlockIndex":0,"delta":{"text":"evidence"}}"#,
            ));
            terminal_prefix.extend(eventstream_frame(
                "messageStop",
                br#"{"stopReason":"end_turn"}"#,
            ));

            if include_metadata {
                let metadata = eventstream_frame(
                    "metadata",
                    br#"{"usage":{"inputTokens":42,"outputTokens":7,"totalTokens":49}}"#,
                );
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nx-amzn-requestid: bedrock-complete\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write chunked headers");
                socket
                    .write_all(format!("{:x}\r\n", terminal_prefix.len()).as_bytes())
                    .await
                    .expect("write first chunk length");
                socket
                    .write_all(&terminal_prefix)
                    .await
                    .expect("write terminal prefix");
                socket.write_all(b"\r\n").await.expect("finish first chunk");
                socket.flush().await.expect("flush terminal prefix");
                socket
                    .write_all(format!("{:x}\r\n", metadata.len()).as_bytes())
                    .await
                    .expect("write metadata chunk length");
                socket.write_all(&metadata).await.expect("write metadata");
                socket
                    .write_all(b"\r\n0\r\n\r\n")
                    .await
                    .expect("finish chunked response");
            } else {
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nx-amzn-requestid: bedrock-partial\r\nConnection: close\r\n\r\n",
                            terminal_prefix.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write fixed headers");
                socket
                    .write_all(&terminal_prefix)
                    .await
                    .expect("write incomplete stream");
            }
            socket.shutdown().await.expect("close Bedrock fixture");
        });
        format!("http://{address}")
    }

    async fn fixture_response(include_metadata: bool) -> reqwest::Response {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct Bedrock fixture client")
            .post(spawn_bedrock_stream(include_metadata).await)
            .send()
            .await
            .expect("request Bedrock fixture")
    }

    async fn stalled_body_response() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled Bedrock fixture");
        let address = listener.local_addr().expect("stalled fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept stalled request");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write stalled headers");
            socket.flush().await.expect("flush stalled headers");
            std::future::pending::<()>().await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct Bedrock fixture client")
            .post(format!("http://{address}"))
            .send()
            .await
            .expect("request stalled Bedrock fixture")
    }

    async fn tool_start_then_stalled_body_response() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tool-start Bedrock fixture");
        let address = listener.local_addr().expect("tool-start fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept tool-start request");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let frame = eventstream_frame(
                "contentBlockStart",
                br#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tool-1","name":"read_file"}}}"#,
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write tool-start headers");
            socket
                .write_all(format!("{:x}\r\n", frame.len()).as_bytes())
                .await
                .expect("write tool-start chunk length");
            socket
                .write_all(&frame)
                .await
                .expect("write tool-start frame");
            socket
                .write_all(b"\r\n")
                .await
                .expect("finish tool-start chunk");
            socket.flush().await.expect("flush tool-start frame");
            std::future::pending::<()>().await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct tool-start Bedrock fixture client")
            .post(format!("http://{address}"))
            .send()
            .await
            .expect("request tool-start Bedrock fixture")
    }

    async fn tool_start_then_repeated_empty_deltas_response() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind repeated-empty-delta Bedrock fixture");
        let address = listener
            .local_addr()
            .expect("repeated-empty-delta fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("accept repeated-empty-delta request");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write repeated-empty-delta headers");

            let start = eventstream_frame(
                "contentBlockStart",
                br#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tool-1","name":"read_file"}}}"#,
            );
            socket
                .write_all(format!("{:x}\r\n", start.len()).as_bytes())
                .await
                .expect("write tool-start chunk length");
            socket
                .write_all(&start)
                .await
                .expect("write tool-start frame");
            socket
                .write_all(b"\r\n")
                .await
                .expect("finish tool-start chunk");

            let empty_delta = eventstream_frame(
                "contentBlockDelta",
                br#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":""}}}"#,
            );
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                if socket
                    .write_all(format!("{:x}\r\n", empty_delta.len()).as_bytes())
                    .await
                    .is_err()
                    || socket.write_all(&empty_delta).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                    || socket.flush().await.is_err()
                {
                    break;
                }
            }
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build repeated-empty-delta Bedrock fixture client")
            .post(format!("http://{address}"))
            .send()
            .await
            .expect("request repeated-empty-delta Bedrock fixture")
    }

    async fn reasoning_then_stalled_body_response() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind reasoning fixture");
        let address = listener.local_addr().expect("reasoning fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept Bedrock request");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            let frame = eventstream_frame(
                "contentBlockDelta",
                br#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"working through the task"}}}"#,
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write reasoning headers");
            socket
                .write_all(format!("{:x}\r\n", frame.len()).as_bytes())
                .await
                .expect("write reasoning chunk length");
            socket
                .write_all(&frame)
                .await
                .expect("write reasoning frame");
            socket
                .write_all(b"\r\n")
                .await
                .expect("finish reasoning chunk");
            socket.flush().await.expect("flush reasoning frame");
            std::future::pending::<()>().await;
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build direct reasoning fixture client")
            .post(format!("http://{address}"))
            .send()
            .await
            .expect("request reasoning fixture")
    }

    async fn reasoning_activity_then_complete_response() -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind progressing reasoning fixture");
        let address = listener
            .local_addr()
            .expect("progressing reasoning fixture address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept Bedrock request");
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write progressing reasoning headers");
            for _ in 0..3 {
                let frame = eventstream_frame(
                    "contentBlockDelta",
                    br#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"thinking"}}}"#,
                );
                socket
                    .write_all(format!("{:x}\r\n", frame.len()).as_bytes())
                    .await
                    .expect("write reasoning chunk length");
                socket
                    .write_all(&frame)
                    .await
                    .expect("write reasoning frame");
                socket
                    .write_all(b"\r\n")
                    .await
                    .expect("finish reasoning chunk");
                socket.flush().await.expect("flush reasoning frame");
                tokio::time::sleep(std::time::Duration::from_millis(8)).await;
            }
            for frame in [
                eventstream_frame(
                    "contentBlockDelta",
                    br#"{"contentBlockIndex":0,"delta":{"text":"done"}}"#,
                ),
                eventstream_frame("messageStop", br#"{"stopReason":"end_turn"}"#),
                eventstream_frame(
                    "metadata",
                    br#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
                ),
            ] {
                socket
                    .write_all(format!("{:x}\r\n", frame.len()).as_bytes())
                    .await
                    .expect("write completion chunk length");
                socket
                    .write_all(&frame)
                    .await
                    .expect("write completion frame");
                socket
                    .write_all(b"\r\n")
                    .await
                    .expect("finish completion chunk");
            }
            socket.write_all(b"0\r\n\r\n").await.expect("finish body");
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build progressing reasoning fixture client")
            .post(format!("http://{address}"))
            .send()
            .await
            .expect("request progressing reasoning fixture")
    }

    #[tokio::test]
    async fn drains_usage_metadata_delivered_after_message_stop() {
        let response = fixture_response(true).await;
        let result = collect_bedrock_stream(
            response,
            "bedrock-test-model",
            Instant::now(),
            LlmCancel::None,
            std::time::Duration::from_secs(1),
            None,
        )
        .await
        .expect("complete Bedrock stream");

        assert_eq!(result.response_id.as_deref(), Some("bedrock-complete"));
        assert_eq!(result.full_text, "evidence");
        assert_eq!(result.usage["input_tokens"], 42);
        assert_eq!(result.usage["output_tokens"], 7);
    }

    #[tokio::test]
    async fn message_stop_without_usage_is_partial_delivery_not_success() {
        let response = fixture_response(false).await;
        let error = collect_bedrock_stream(
            response,
            "bedrock-test-model",
            Instant::now(),
            LlmCancel::None,
            std::time::Duration::from_secs(1),
            None,
        )
        .await
        .expect_err("missing usage must not become a successful inference");

        let BedrockStreamError::Transport { error, partial } = error else {
            panic!("expected partial-delivery transport error")
        };
        assert!(error.contains("trailing usage metadata"), "{error}");
        assert_eq!(partial.response_id.as_deref(), Some("bedrock-partial"));
        assert_eq!(partial.full_text, "evidence");
        assert!(partial.usage.is_empty());
    }

    #[tokio::test]
    async fn cancellation_interrupts_stalled_body_before_idle_timeout() {
        let response = stalled_body_response().await;
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_signal = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel_signal.cancel();
        });
        let started = Instant::now();

        let error = collect_bedrock_stream(
            response,
            "bedrock-test-model",
            started,
            LlmCancel::Token(&cancel),
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .expect_err("cancel must win over a stalled Bedrock body");

        assert!(matches!(error, BedrockStreamError::Cancelled { .. }));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn selected_tool_enters_bedrock_delivery_and_partial_json_times_out_safely() {
        let response = tool_start_then_stalled_body_response().await;
        let authorized = HashSet::from(["read_file".to_string()]);
        let error = collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
            response,
            "bedrock-test-model",
            Instant::now(),
            std::time::Duration::from_secs(30),
            LlmCancel::None,
            std::time::Duration::from_millis(60),
            std::time::Duration::from_millis(20),
            Some(&authorized),
            None,
        )
        .await
        .expect_err(
            "a selected tool with stalled arguments remains under the delivery-yield deadline",
        );

        let BedrockStreamError::SemanticProgressTimeout {
            made_semantic_progress,
            partial,
            ..
        } = error
        else {
            panic!("expected safe Bedrock partial deadline")
        };
        assert!(
            made_semantic_progress,
            "valid tool selection starts delivery"
        );
        assert!(partial.full_text.is_empty());
    }

    #[tokio::test]
    async fn repeated_bedrock_empty_tool_fragments_do_not_extend_delivery_deadline() {
        let response = tool_start_then_repeated_empty_deltas_response().await;
        let authorized = HashSet::from(["read_file".to_string()]);
        let error = collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
            response,
            "bedrock-test-model",
            Instant::now(),
            std::time::Duration::from_secs(1),
            LlmCancel::None,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(20),
            Some(&authorized),
            None,
        )
        .await
        .expect_err("empty Bedrock tool deltas must not keep delivery alive");

        assert!(matches!(
            error,
            BedrockStreamError::SemanticProgressTimeout {
                made_semantic_progress: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn reasoning_then_silence_still_hits_bedrock_activity_deadline() {
        let response = reasoning_then_stalled_body_response().await;
        let error = collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
            response,
            "bedrock-test-model",
            Instant::now(),
            std::time::Duration::from_secs(30),
            LlmCancel::None,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(10),
            None,
            None,
        )
        .await
        .expect_err("a reasoning stream that goes silent must still time out");

        let BedrockStreamError::SemanticProgressTimeout {
            made_semantic_progress,
            partial,
            ..
        } = error
        else {
            panic!("expected Bedrock activity deadline");
        };
        assert!(!made_semantic_progress);
        assert_eq!(partial.reasoning, "working through the task");
        assert!(partial.full_text.is_empty());
    }

    #[tokio::test]
    async fn continuous_bedrock_reasoning_refreshes_liveness_until_delivery() {
        let response = reasoning_activity_then_complete_response().await;
        let result = collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
            response,
            "bedrock-test-model",
            Instant::now(),
            std::time::Duration::from_secs(1),
            LlmCancel::None,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(12),
            None,
            None,
        )
        .await
        .expect("continuous Bedrock reasoning must not look idle");

        assert_eq!(result.full_text, "done");
        assert_eq!(result.reasoning, "thinkingthinkingthinking");
    }

    #[tokio::test]
    async fn bedrock_reasoning_cannot_bypass_provider_work_budget() {
        let response = reasoning_then_stalled_body_response().await;
        let error = collect_bedrock_stream_with_semantic_progress_deadline_and_surface(
            response,
            "bedrock-test-model",
            Instant::now(),
            std::time::Duration::from_millis(20),
            LlmCancel::None,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
            None,
            None,
        )
        .await
        .expect_err("semantic progress must not bypass the Bedrock work budget");

        let BedrockStreamError::ProviderWorkDeadline { partial, .. } = error else {
            panic!("expected Bedrock provider-work deadline")
        };
        assert_eq!(partial.reasoning, "working through the task");
    }
}
