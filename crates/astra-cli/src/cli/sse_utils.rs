//! Work CLI SSE text rendering and error summaries.
//!
//! Work turns use this parser to display streamed output and surface transport
//! failures. The Work caller owns controller release and terminal error events.

use crate::cli::theme;
use futures_util::StreamExt;
use std::io::IsTerminal;

/// Maximum SSE buffer size (1 MB). If a malformed stream sends data without
/// `\n\n` delimiters, we truncate the buffer to prevent unbounded memory growth.
const MAX_SSE_BUFFER: usize = 1024 * 1024;

#[inline]
fn trace_sse_buffer_truncated() {
    tracing::warn!(
        target: "astra_cli::sse",
        max_bytes = MAX_SSE_BUFFER,
        "sse buffer exceeded; truncated incomplete events"
    );
}

#[inline]
fn trace_sse_server_error_event(message: &str) {
    tracing::warn!(
        target: "astra_cli::sse",
        message = %message,
        "sse server error event"
    );
}

/// Outcome of collecting text from an SSE stream.
pub struct SseTextResult {
    pub text: String,
    pub event_count: usize,
    /// Distinct event types seen (e.g. `["text_delta", "error"]`).
    pub event_types: Vec<String>,
    /// Transport-level failure while reading the SSE body.
    pub stream_error: Option<String>,
    /// True when we had to truncate an oversized malformed SSE buffer.
    pub truncated: bool,
}

impl SseTextResult {
    pub fn completion_error(&self) -> Option<String> {
        self.stream_error.clone().or_else(|| {
            self.truncated.then(|| {
                format!("SSE buffer exceeded {MAX_SSE_BUFFER} bytes before a complete event")
            })
        })
    }
}

/// Stream SSE text through [`StreamingMarkdown`] for real-time rendered output.
///
/// Feeds each `text_delta` into the markdown renderer on a terminal and writes
/// plain text to stderr otherwise. Returns text, event types and read failures
/// for the Work caller to inspect after releasing control.
pub async fn stream_sse_markdown(resp: reqwest::Response) -> SseTextResult {
    let use_md = std::io::stdout().is_terminal();
    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);

    let mut md = if use_md {
        Some(crate::cli::stream::streaming_md::StreamingMarkdown::new(tw))
    } else {
        None
    };

    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
        stream_error: None,
        truncated: false,
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target: "astra_cli::sse", error = %e, "sse stream read failed");
                result.stream_error = Some(format!("SSE stream read failed: {e}"));
                break;
            }
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // Guard against unbounded buffer growth from malformed streams
        if buffer.len() > MAX_SSE_BUFFER {
            eprintln!(
                "\r  {} SSE buffer exceeded {} bytes, truncating incomplete events",
                theme::icon_warn(),
                MAX_SSE_BUFFER
            );
            trace_sse_buffer_truncated();
            result.truncated = true;
            buffer.clear();
            break;
        }

        while let Some(event_end) = buffer.find("\n\n") {
            let event_str = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            for line in event_str.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    result.event_count += 1;
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = json
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        if !result.event_types.contains(&event_type.to_string()) {
                            result.event_types.push(event_type.to_string());
                        }

                        match event_type {
                            "text_delta" => {
                                if let Some(content) = json.get("content").and_then(|v| v.as_str())
                                {
                                    result.text.push_str(content);
                                    if let Some(ref mut renderer) = md {
                                        renderer.push(content);
                                    } else {
                                        eprint!("{}", content);
                                    }
                                }
                            }
                            "error" => {
                                if let Some(msg) = json
                                    .get("message")
                                    .or_else(|| json.get("error"))
                                    .and_then(|v| v.as_str())
                                {
                                    eprintln!("\r  {} Server error: {}", theme::icon_err(), msg);
                                    trace_sse_server_error_event(msg);
                                    if result.stream_error.is_none()
                                        && astra_turn_core::chat_turn_heuristics::is_session_not_found_error(msg)
                                    {
                                        result.stream_error = Some(msg.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Drain remaining buffer
    for line in buffer.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            result.event_count += 1;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                && json.get("type").and_then(|v| v.as_str()) == Some("text_delta")
                && let Some(content) = json.get("content").and_then(|v| v.as_str())
            {
                result.text.push_str(content);
                if let Some(ref mut renderer) = md {
                    renderer.push(content);
                } else {
                    eprint!("{}", content);
                }
            }
        }
    }

    if let Some(ref mut renderer) = md {
        renderer.finish();
    }
    // Ensure a newline after the streamed block
    if !result.text.is_empty() {
        eprintln!();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{MAX_SSE_BUFFER, stream_sse_markdown};
    use http::Response;

    fn sse_response(body: &'static str) -> reqwest::Response {
        let r = Response::builder()
            .status(200)
            .body(reqwest::Body::from(body))
            .expect("test response");
        reqwest::Response::from(r)
    }

    fn sse_error_response() -> reqwest::Response {
        let body = reqwest::Body::wrap_stream(futures_util::stream::once(async {
            Err::<Vec<u8>, std::io::Error>(std::io::Error::other("boom"))
        }));
        let r = Response::builder()
            .status(200)
            .body(body)
            .expect("test response");
        reqwest::Response::from(r)
    }

    #[tokio::test]
    async fn stream_sse_markdown_reports_stream_read_errors() {
        let r = stream_sse_markdown(sse_error_response()).await;
        assert_eq!(r.text, "");
        assert!(
            r.completion_error()
                .is_some_and(|msg| msg.contains("SSE stream read failed"))
        );
    }

    #[tokio::test]
    async fn stream_sse_markdown_reports_buffer_truncation() {
        let oversized = format!(
            "data: {{\"type\":\"text_delta\",\"content\":\"{}\n",
            "x".repeat(MAX_SSE_BUFFER)
        );
        let response = Response::builder()
            .status(200)
            .body(reqwest::Body::from(oversized))
            .expect("test response");
        let r = stream_sse_markdown(reqwest::Response::from(response)).await;
        assert!(r.truncated);
        assert!(r.completion_error().is_some());
    }

    #[tokio::test]
    async fn stream_sse_markdown_merges_text_deltas() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"hel\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"lo\"}\n\n",
        );
        let r = stream_sse_markdown(sse_response(body)).await;
        assert_eq!(r.text, "hello");
        assert!(r.event_types.contains(&"text_delta".to_string()));
        assert!(r.event_count >= 2);
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn stream_sse_markdown_records_error_type() {
        let body = "data: {\"type\":\"error\",\"message\":\"bad\"}\n\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert!(r.event_types.contains(&"error".to_string()));
        assert!(r.text.is_empty());
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn stream_sse_markdown_tail_buffer_text_delta() {
        let body = "data: {\"type\":\"text_delta\",\"content\":\"x\"}\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert_eq!(r.text, "x");
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn stream_sse_markdown_records_mixed_events() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"a\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"b\"}\n\n",
            "data: {\"type\":\"error\",\"message\":\"x\"}\n\n",
        );
        let streamed = stream_sse_markdown(sse_response(body)).await;
        assert_eq!(streamed.text, "ab");
        assert_eq!(streamed.event_types, ["text_delta", "error"]);
        assert_eq!(streamed.event_count, 3);
        assert!(streamed.completion_error().is_none());
    }

    #[tokio::test]
    async fn stream_sse_markdown_session_not_found_promotes_to_stream_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"Session not found\"}\n\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert!(
            r.completion_error().is_some(),
            "Session not found should be promoted to stream_error"
        );
        assert!(r.completion_error().unwrap().contains("Session not found"),);
    }

    #[tokio::test]
    async fn stream_sse_markdown_generic_error_does_not_promote_to_stream_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"rate limit exceeded\"}\n\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert!(
            r.completion_error().is_none(),
            "generic errors should not be promoted to stream_error"
        );
    }
}
