//! Shared SSE parsing for CLI code paths that consume `text_delta` streams.
//!
//! **Single source of truth:** `collect_sse_text` and `stream_sse_markdown` live only here.
//! `plan_interaction` and `slash_memory` import them — do not duplicate the
//! `data:` / `text_delta` loop elsewhere.

use crate::theme;
use futures_util::StreamExt;
use std::io::IsTerminal;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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
    /// Session ID from `session_info` event (if present).
    pub session_id: Option<String>,
    /// True when the stream was interrupted by a cancellation token.
    pub cancelled: bool,
}

impl SseTextResult {
    pub fn completion_error(&self) -> Option<String> {
        self.stream_error.clone().or_else(|| {
            self.truncated.then(|| {
                format!("SSE buffer exceeded {MAX_SSE_BUFFER} bytes before a complete event")
            })
        })
    }

    /// True when the stream was interrupted by a cancellation token.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// Collect text from an SSE response: `data: {...}` lines, `text_delta` → `text`, `error` → stderr.
///
/// When `stream_to_stderr` is true, prints text deltas as they arrive.
pub async fn collect_sse_text(resp: reqwest::Response, stream_to_stderr: bool) -> SseTextResult {
    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
        stream_error: None,
        truncated: false,
        session_id: None,
        cancelled: false,
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
                                    if stream_to_stderr {
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

    for line in buffer.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            result.event_count += 1;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                && json.get("type").and_then(|v| v.as_str()) == Some("text_delta")
                && let Some(content) = json.get("content").and_then(|v| v.as_str())
            {
                result.text.push_str(content);
                if stream_to_stderr {
                    eprint!("{}", content);
                }
            }
        }
    }

    result
}

/// Stream SSE text through [`StreamingMarkdown`] for real-time rendered output.
///
/// Behaves like `collect_sse_text` but feeds each `text_delta` chunk into the
/// streaming markdown renderer so the user sees the same incremental rendering
/// as chat mode.  Falls back to plain `eprint!` when stdout is not a terminal.
///
/// Returns the accumulated text (same as `collect_sse_text`) so callers can
/// parse it afterwards (e.g. plan JSON extraction).
pub async fn stream_sse_markdown(resp: reqwest::Response) -> SseTextResult {
    let use_md = std::io::stdout().is_terminal();
    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);

    let mut md = if use_md {
        Some(super::streaming_md::StreamingMarkdown::new(tw))
    } else {
        None
    };

    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
        stream_error: None,
        truncated: false,
        session_id: None,
        cancelled: false,
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

/// Collect SSE text while showing a live thinking-style preview pane.
///
/// Feeds each `text_delta` chunk into a [`ThinkingPreviewPane`] so the user
/// sees the LLM's output streaming in real-time. When the stream ends the
/// pane is cleared and a one-line summary is printed (word count + elapsed).
///
/// Returns the accumulated text (same as `collect_sse_text`) so callers can
/// parse it afterwards (e.g. plan JSON extraction).
pub async fn collect_sse_with_preview(resp: reqwest::Response) -> SseTextResult {
    use super::effects::{ThinkingPreviewPane, thinking_viewport_rows};

    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let rows = thinking_viewport_rows();
    let mut pane = ThinkingPreviewPane::new(rows, tw);

    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
        stream_error: None,
        truncated: false,
        session_id: None,
        cancelled: false,
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
                                    pane.push_chunk(content);
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
                pane.push_chunk(content);
            }
        }
    }

    // Show summary, then clear the preview
    let summary = pane.summary_line();
    pane.clear();
    if !summary.is_empty() {
        eprintln!("{}", summary);
    }

    result
}

/// Cancellable SSE collection with timeout, preview pane, and progress callback.
///
/// Like [`collect_sse_with_preview`] but supports:
/// - **Cancellation** via [`CancellationToken`] (wired to Ctrl-C by caller)
/// - **Stream timeout** — total wall-clock limit for the entire SSE stream
/// - **Idle timeout** — max gap between consecutive SSE data frames
/// - **Progress callback** — called on each `text_delta` chunk
///
/// Used by plan generation where the user must be able to interrupt long LLM calls.
pub async fn collect_sse_cancellable(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    stream_timeout: Duration,
    idle_timeout: Duration,
    mut on_chunk: impl FnMut(&str),
) -> SseTextResult {
    use super::effects::{ThinkingPreviewPane, thinking_viewport_rows};

    let tw = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let rows = thinking_viewport_rows();
    let mut pane = ThinkingPreviewPane::new(rows, tw);

    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
        stream_error: None,
        truncated: false,
        session_id: None,
        cancelled: false,
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();
    let deadline = tokio::time::Instant::now() + stream_timeout;
    let mut last_data = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_data + idle_timeout;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::debug!(target: "astra_cli::sse", "sse plan stream cancelled");
                result.stream_error = Some("Plan generation cancelled".into());
                result.cancelled = true;
                break;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let secs = stream_timeout.as_secs();
                tracing::warn!(target: "astra_cli::sse", stream_timeout_secs = secs, "sse stream wall-clock timeout");
                result.stream_error = Some(format!(
                    "Stream timeout after {}s",
                    secs
                ));
                break;
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                let secs = idle_timeout.as_secs();
                tracing::warn!(target: "astra_cli::sse", idle_timeout_secs = secs, "sse stream idle timeout");
                result.stream_error = Some(format!(
                    "No data received for {}s",
                    secs
                ));
                break;
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        last_data = tokio::time::Instant::now();
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

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
                                                if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                                                    result.text.push_str(content);
                                                    pane.push_chunk(content);
                                                    on_chunk(content);
                                                }
                                            }
                                            "session_info" => {
                                                if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                                                    result.session_id = Some(sid.to_string());
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
                    Some(Err(e)) => {
                        tracing::warn!(target: "astra_cli::sse", error = %e, "sse stream read failed");
                        result.stream_error = Some(format!("SSE stream read failed: {e}"));
                        break;
                    }
                    None => break, // stream ended normally
                }
            }
        }
    }

    // Drain remaining buffer
    for line in buffer.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            result.event_count += 1;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                match json.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                            result.text.push_str(content);
                            pane.push_chunk(content);
                            on_chunk(content);
                        }
                    }
                    Some("session_info") => {
                        if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                            result.session_id = Some(sid.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let summary = pane.summary_line();
    pane.clear();
    if !summary.is_empty() {
        eprintln!("{}", summary);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn collect_sse_merges_text_deltas() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"hel\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"lo\"}\n\n",
        );
        let r = collect_sse_text(sse_response(body), false).await;
        assert_eq!(r.text, "hello");
        assert!(r.event_types.contains(&"text_delta".to_string()));
        assert!(r.event_count >= 2);
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn collect_sse_records_error_type() {
        let body = "data: {\"type\":\"error\",\"message\":\"bad\"}\n\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert!(r.event_types.contains(&"error".to_string()));
        assert!(r.text.is_empty());
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn collect_sse_tail_buffer_text_delta() {
        // No trailing \n\n — handled by final line scan
        let body = "data: {\"type\":\"text_delta\",\"content\":\"x\"}\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert_eq!(r.text, "x");
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn collect_sse_reports_stream_read_errors() {
        let r = collect_sse_text(sse_error_response(), false).await;
        assert_eq!(r.text, "");
        assert!(
            r.completion_error()
                .is_some_and(|msg| msg.contains("SSE stream read failed"))
        );
    }

    #[tokio::test]
    async fn collect_sse_reports_buffer_truncation() {
        let oversized = format!(
            "data: {{\"type\":\"text_delta\",\"content\":\"{}\n",
            "x".repeat(MAX_SSE_BUFFER)
        );
        let response = Response::builder()
            .status(200)
            .body(reqwest::Body::from(oversized))
            .expect("test response");
        let r = collect_sse_text(reqwest::Response::from(response), false).await;
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
    async fn stream_sse_markdown_matches_collect_sse_text() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"a\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"b\"}\n\n",
            "data: {\"type\":\"error\",\"message\":\"x\"}\n\n",
        );
        let collected = collect_sse_text(sse_response(body), false).await;
        let streamed = stream_sse_markdown(sse_response(body)).await;
        assert_eq!(streamed.text, collected.text, "accumulated text must match");
        assert_eq!(
            streamed.event_types, collected.event_types,
            "event type set must match collect_sse_text"
        );
        assert_eq!(
            streamed.event_count, collected.event_count,
            "event_count must stay in sync with collect_sse_text"
        );
        assert_eq!(
            streamed.completion_error(),
            collected.completion_error(),
            "terminal failure state must match collect_sse_text"
        );
    }

    // ── collect_sse_cancellable tests ───────────────────────────────────

    #[tokio::test]
    async fn cancellable_collects_text_deltas() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"hel\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"lo\"}\n\n",
        );
        let cancel = CancellationToken::new();
        let r = collect_sse_cancellable(
            sse_response(body),
            &cancel,
            Duration::from_secs(10),
            Duration::from_secs(5),
            |_| {},
        )
        .await;
        assert_eq!(r.text, "hello");
        assert!(!r.is_cancelled());
        assert!(r.completion_error().is_none());
    }

    #[tokio::test]
    async fn cancellable_respects_cancellation_token() {
        // Stream that never ends — cancellation must break out
        let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(0u32, |i| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let chunk = format!("data: {{\"type\":\"text_delta\",\"content\":\"chunk{i}\"}}\n\n");
            Some((Ok::<_, std::io::Error>(chunk.into_bytes()), i + 1))
        }));
        let resp = reqwest::Response::from(
            Response::builder()
                .status(200)
                .body(body)
                .expect("test response"),
        );

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_clone.cancel();
        });

        let r = collect_sse_cancellable(
            resp,
            &cancel,
            Duration::from_secs(60),
            Duration::from_secs(60),
            |_| {},
        )
        .await;
        assert!(r.is_cancelled(), "should be cancelled");
        assert!(
            !r.text.is_empty(),
            "should have collected some chunks before cancel"
        );
    }

    #[tokio::test]
    async fn cancellable_idle_timeout_fires() {
        // Stream that sends one chunk then stalls
        let body =
            reqwest::Body::wrap_stream(futures_util::stream::unfold(false, |sent| async move {
                if sent {
                    tokio::time::sleep(Duration::from_secs(300)).await;
                    None
                } else {
                    let chunk = "data: {\"type\":\"text_delta\",\"content\":\"x\"}\n\n";
                    Some((Ok::<_, std::io::Error>(chunk.as_bytes().to_vec()), true))
                }
            }));
        let resp = reqwest::Response::from(
            Response::builder()
                .status(200)
                .body(body)
                .expect("test response"),
        );

        let cancel = CancellationToken::new();
        let r = collect_sse_cancellable(
            resp,
            &cancel,
            Duration::from_secs(60),
            Duration::from_millis(200),
            |_| {},
        )
        .await;
        assert_eq!(r.text, "x");
        assert!(
            r.completion_error()
                .is_some_and(|e| e.contains("No data received")),
            "should report idle timeout, got: {:?}",
            r.completion_error()
        );
    }

    #[tokio::test]
    async fn cancellable_calls_on_chunk() {
        let body = concat!(
            "data: {\"type\":\"text_delta\",\"content\":\"a\"}\n\n",
            "data: {\"type\":\"text_delta\",\"content\":\"b\"}\n\n",
        );
        let cancel = CancellationToken::new();
        let mut chunks = Vec::new();
        let r = collect_sse_cancellable(
            sse_response(body),
            &cancel,
            Duration::from_secs(10),
            Duration::from_secs(5),
            |c| chunks.push(c.to_string()),
        )
        .await;
        assert_eq!(r.text, "ab");
        assert_eq!(chunks, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn is_cancelled_returns_false_for_other_errors() {
        let r = collect_sse_text(sse_error_response(), false).await;
        assert!(!r.is_cancelled());
        assert!(!r.cancelled);
    }

    #[test]
    fn is_cancelled_uses_bool_not_string() {
        let mut r = SseTextResult {
            text: String::new(),
            event_count: 0,
            event_types: vec![],
            stream_error: Some("Plan generation cancelled".into()),
            truncated: false,
            session_id: None,
            cancelled: false,
        };
        // Even with "cancelled" in the error string, is_cancelled is false
        // because the bool field is what matters
        assert!(!r.is_cancelled());
        r.cancelled = true;
        assert!(r.is_cancelled());
    }

    #[tokio::test]
    async fn collect_sse_session_not_found_promotes_to_stream_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"Session not found\"}\n\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert!(
            r.completion_error().is_some(),
            "Session not found should be promoted to stream_error"
        );
        assert!(r.completion_error().unwrap().contains("Session not found"),);
    }

    #[tokio::test]
    async fn collect_sse_generic_error_does_not_promote_to_stream_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"rate limit exceeded\"}\n\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert!(
            r.completion_error().is_none(),
            "generic errors should not be promoted to stream_error"
        );
    }
}
