//! Shared SSE parsing for CLI code paths that consume `text_delta` streams.
//!
//! **Single source of truth:** `collect_sse_text` lives only here. `plan_interaction` and
//! `slash_memory` import it — do not duplicate the `data:` / `text_delta` loop elsewhere.

use crate::theme;
use futures_util::StreamExt;

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
}
