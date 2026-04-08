//! Shared SSE parsing for CLI code paths that consume `text_delta` streams.
//!
//! **Single source of truth:** `collect_sse_text` and `stream_sse_markdown` live only here.
//! `plan_interaction` and `slash_memory` import them — do not duplicate the
//! `data:` / `text_delta` loop elsewhere.

use crate::theme;
use futures_util::StreamExt;
use std::io::IsTerminal;

/// Maximum SSE buffer size (1 MB). If a malformed stream sends data without
/// `\n\n` delimiters, we truncate the buffer to prevent unbounded memory growth.
const MAX_SSE_BUFFER: usize = 1024 * 1024;

/// Outcome of collecting text from an SSE stream.
pub struct SseTextResult {
    pub text: String,
    pub event_count: usize,
    /// Distinct event types seen (e.g. `["text_delta", "error"]`).
    pub event_types: Vec<String>,
}

/// Collect text from an SSE response: `data: {...}` lines, `text_delta` → `text`, `error` → stderr.
///
/// When `stream_to_stderr` is true, prints text deltas as they arrive.
pub async fn collect_sse_text(
    resp: reqwest::Response,
    stream_to_stderr: bool,
) -> SseTextResult {
    let mut result = SseTextResult {
        text: String::new(),
        event_count: 0,
        event_types: Vec::new(),
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // Guard against unbounded buffer growth from malformed streams
        if buffer.len() > MAX_SSE_BUFFER {
            eprintln!(
                "\r  {} SSE buffer exceeded {} bytes, truncating incomplete events",
                theme::icon_warn(),
                MAX_SSE_BUFFER
            );
            buffer.clear();
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
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // Guard against unbounded buffer growth from malformed streams
        if buffer.len() > MAX_SSE_BUFFER {
            eprintln!(
                "\r  {} SSE buffer exceeded {} bytes, truncating incomplete events",
                theme::icon_warn(),
                MAX_SSE_BUFFER
            );
            buffer.clear();
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
                                if let Some(content) =
                                    json.get("content").and_then(|v| v.as_str())
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
                                    eprintln!(
                                        "\r  {} Server error: {}",
                                        theme::icon_err(),
                                        msg
                                    );
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
    };
    let mut buffer = String::new();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        if buffer.len() > MAX_SSE_BUFFER {
            buffer.clear();
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
                                if let Some(content) =
                                    json.get("content").and_then(|v| v.as_str())
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
                                    eprintln!(
                                        "\r  {} Server error: {}",
                                        theme::icon_err(),
                                        msg
                                    );
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
    }

    #[tokio::test]
    async fn collect_sse_records_error_type() {
        let body = "data: {\"type\":\"error\",\"message\":\"bad\"}\n\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert!(r.event_types.contains(&"error".to_string()));
        assert!(r.text.is_empty());
    }

    #[tokio::test]
    async fn collect_sse_tail_buffer_text_delta() {
        // No trailing \n\n — handled by final line scan
        let body = "data: {\"type\":\"text_delta\",\"content\":\"x\"}\n";
        let r = collect_sse_text(sse_response(body), false).await;
        assert_eq!(r.text, "x");
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
    }

    #[tokio::test]
    async fn stream_sse_markdown_records_error_type() {
        let body = "data: {\"type\":\"error\",\"message\":\"bad\"}\n\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert!(r.event_types.contains(&"error".to_string()));
        assert!(r.text.is_empty());
    }

    #[tokio::test]
    async fn stream_sse_markdown_tail_buffer_text_delta() {
        let body = "data: {\"type\":\"text_delta\",\"content\":\"x\"}\n";
        let r = stream_sse_markdown(sse_response(body)).await;
        assert_eq!(r.text, "x");
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
    }
}
